//! 设备枚举，以及「打开流时到底协商到了什么参数」。
//!
//! # 为什么这一层不能省
//!
//! 整条 DSP 链的系数都在运行时按**实际达成**的采样率生成。如果这里读到
//! 的采样率和流真正跑的那个不一样，A 计权、八度带、触发带的中心频率会
//! **整体偏移**，而结果看起来完全正常——只显示出来的 dB 数是错的。
//!
//! 这件事在本项目里是必然会遇到的，不是理论风险：
//!
//! * **WASAPI 共享模式会静默重采样**。你请求 48 kHz，设备以 44.1 kHz
//!   为本，实际跑的就是 44.1 kHz。
//! * **插上蓝牙耳机改变默认输出设备，可能连带改变默认输入设备**，
//!   采样率随之掉到 16 kHz（macOS 会切到 HFP 通话模式：8/16 kHz +
//!   AGC + 高通，同时毁掉录制和回放）。
//! * **Windows 的「音频增强」**会在驱动层插一个高通，而设备自己并不
//!   知道，也不会在能力列表里体现。这就是 `docs/manual-test.md` 里那条
//!   必须手动做的 A/B 检查存在的原因。
//!
//! 所以这里的返回值会被一路带到 UI 和 session manifest 上，**用户能看见
//! 我们实际用的是什么**，而不是我们以为请求了什么。
//!
//! # 采样率下限
//!
//! 16 kHz 是硬下限（[`MIN_SAMPLE_RATE`]）。A 计权在 12194 Hz 有极点，
//! 低于 16 kHz 时它高于 Nyquist，预畸变公式里的 `tan(πf/fs)` 会变号，
//! 滤波器要么被钳位（那一带的 dB(A) 就是错的），要么直接不稳定。
//! **拒绝运行比降级运行诚实**——降级运行时报出来的 dB(A) 是错的，
//! 而用户不会知道。

use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{
    BufferSize, Device, DeviceId, FrameCount, SampleFormat, StreamConfig, SupportedBufferSize,
    SupportedStreamConfig, SupportedStreamConfigRange,
};
use noise_core::CoreError;
use serde::{Deserialize, Serialize};

/// 可用的采样率下限，Hz。
///
/// 见模块文档。低于这个值**直接拒绝启动监测**，不做任何降级。
pub const MIN_SAMPLE_RATE: u32 = 16_000;

/// 默认流参数。
///
/// 缓冲大小交给后端决定（`BufferSize::Default`）。WASAPI 共享模式的典型
/// 值是 10 ms，CoreAudio 更小。**不要为了「降低延迟」去写死一个小的固定
/// 值**：后端要么拒绝，要么接受之后在系统负载高时周期性断流，而断流的
/// 表现是音频出爆音——比多 10 ms 延迟糟得多。
pub const DEFAULT_BUFFER_FRAMES: Option<FrameCount> = None;

// ---------------------------------------------------------------------------
// 采样格式的编号
// ---------------------------------------------------------------------------

/// 未知格式的编号。
///
/// cpal 的 `SampleFormat` 是 `#[non_exhaustive]` 的，将来会加变体。
/// 遇到没见过的变体时用这个值，**而不是随便挑一个已有编号**——把
/// 一个未知格式记成 f32，会让所有录音的电平差 20 dB 以上，而且看不出来。
pub const FORMAT_CODE_UNKNOWN: u8 = 255;

/// `SampleFormat` → 稳定编号。
///
/// 用编号而不是字符串存进原子量，是因为 [`crate::status::MonitorStatus`]
/// 的字段必须是可原子读写的定长类型；而这个编号还要写进 session
/// manifest，所以**一旦发布就不能再改**——它是有线格式。
///
/// 已知变体全部显式列出，不用 `as u8`：`SampleFormat` 是 `#[non_exhaustive]`
/// 的外部枚举，`as u8` 会在 cpal 增加变体时静默移位，而显式 match 加上
/// 一个 `_` 分支能让新变体落进 [`FORMAT_CODE_UNKNOWN`]。
pub fn format_code(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::I8 => 0,
        SampleFormat::I16 => 1,
        SampleFormat::I24 => 2,
        SampleFormat::I32 => 3,
        SampleFormat::I64 => 4,
        SampleFormat::U8 => 5,
        SampleFormat::U16 => 6,
        SampleFormat::U24 => 7,
        SampleFormat::U32 => 8,
        SampleFormat::U64 => 9,
        SampleFormat::F32 => 10,
        SampleFormat::F64 => 11,
        SampleFormat::DsdU8 => 12,
        SampleFormat::DsdU16 => 13,
        SampleFormat::DsdU32 => 14,
        _ => FORMAT_CODE_UNKNOWN,
    }
}

/// 编号 → 名字。给人看的形式，也是写进 manifest 的形式。
pub fn format_name(code: u8) -> &'static str {
    match code {
        0 => "i8",
        1 => "i16",
        2 => "i24",
        3 => "i32",
        4 => "i64",
        5 => "u8",
        6 => "u16",
        7 => "u24",
        8 => "u32",
        9 => "u64",
        10 => "f32",
        11 => "f64",
        12 => "dsd_u8",
        13 => "dsd_u16",
        14 => "dsd_u32",
        _ => "unknown",
    }
}

/// 编号 → 变体。
///
/// 未知编号返回 `None` 而不是猜一个：一个来自未来版本的 manifest
/// 被当成 f32 读进来，会让所有录音的电平差得很远。
pub fn format_from_code(code: u8) -> Option<SampleFormat> {
    Some(match code {
        0 => SampleFormat::I8,
        1 => SampleFormat::I16,
        2 => SampleFormat::I24,
        3 => SampleFormat::I32,
        4 => SampleFormat::I64,
        5 => SampleFormat::U8,
        6 => SampleFormat::U16,
        7 => SampleFormat::U24,
        8 => SampleFormat::U32,
        9 => SampleFormat::U64,
        10 => SampleFormat::F32,
        11 => SampleFormat::F64,
        12 => SampleFormat::DsdU8,
        13 => SampleFormat::DsdU16,
        14 => SampleFormat::DsdU32,
        _ => return None,
    })
}

/// 定点格式每样本的有效位数。浮点是 32 / 64。
///
/// **i24 是 4 字节存储、24 位有效**，所以存储大小和有效位数不是一回事，
/// 读 WAV 头的时候尤其容易搞混。
pub fn format_bits(format: SampleFormat) -> u32 {
    format.bits_per_sample()
}

/// 这种格式能不能用带类型的 `build_*_stream` 打开。
///
/// **DSD 不行**，而且必须显式挡掉：DSD 的 1 bit 不是数值样本，
/// `dasp_sample::Sample` 没有为它们实现，带类型的
/// `build_input_stream::<T, ...>` 根本实例化不出来。DSD 设备会出现在
/// 能力列表里，所以「设备说自己支持 15 种格式」和「我们能打开其中的
/// 12 种」是两件事，UI 上要说清楚。
///
/// 真要支持 DSD 得走 `build_*_stream_raw` 自己解位流——那不是本项目
/// 需要的东西，因为 DSD 的噪声整形把低频段推得很高，对检测 63 Hz
/// 没有任何好处。
pub fn is_capturable(format: SampleFormat) -> bool {
    !matches!(
        format,
        SampleFormat::DsdU8 | SampleFormat::DsdU16 | SampleFormat::DsdU32
    )
}

// ---------------------------------------------------------------------------
// 枚举结果
// ---------------------------------------------------------------------------

/// 设备能接受的一种「声道数 × 采样率范围 × 格式」组合。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatRange {
    pub channels: u16,
    pub min_sample_rate: u32,
    pub max_sample_rate: u32,
    /// [`format_code`] 对应的名字，便于人读 JSON。
    pub format: String,
    /// 稳定编号，见 [`format_code`]。
    pub format_code: u8,
    /// 每样本有效位数。
    pub bits: u32,
    pub buffer: BufferRange,
}

/// 设备能接受的缓冲大小。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "frames")]
pub enum BufferRange {
    /// 后端在流启动之前无从得知。**不是「不支持」**——WASAPI 和
    /// CoreAudio 都落在这里，而它们都能跑。
    Unknown,
    Range { min: u32, max: u32 },
}

impl FormatRange {
    /// 这个组合能不能以给定的采样率跑。
    pub fn allows_rate(&self, rate: u32) -> bool {
        self.min_sample_rate <= rate && rate <= self.max_sample_rate
    }

    /// 这个组合在给定采样率下是否**可用**——即既在范围内，
    /// 又不低于 [`MIN_SAMPLE_RATE`]。
    pub fn usable_at(&self, rate: u32) -> bool {
        rate >= MIN_SAMPLE_RATE && self.allows_rate(rate)
    }

    /// 挑一个采样率。
    ///
    /// 优先 `preferred`（用户在配置里指定的），其次范围内的最高值。
    /// 最高值而不是最低值：设备的高端通常是它原生支持的速率，而低端
    /// 往往是重采样出来的。
    ///
    /// 没有可用速率时返回 `None`——调用方负责把它变成一条给用户看的
    /// 错误，而不是一个默认值。
    pub fn pick_rate(&self, preferred: Option<u32>) -> Option<u32> {
        if let Some(p) = preferred {
            if self.usable_at(p) {
                return Some(p);
            }
        }
        if self.usable_at(self.max_sample_rate) {
            return Some(self.max_sample_rate);
        }
        // 高端低于下限（蓝牙 HFP 的 8 kHz 就落在这里）：看看低端有没有救。
        if self.usable_at(self.min_sample_rate) {
            return Some(self.min_sample_rate);
        }
        None
    }
}

/// 一台设备。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// [`DeviceId`] 的字符串形式。持久化配置里存的就是它。
    pub id: String,
    pub name: String,
    /// 是否是系统当前的默认输入/输出设备。
    pub is_default: bool,
    /// 设备自己推荐的参数（`default_input_config` / `default_output_config`）。
    pub default_format: Option<FormatRange>,
    /// 全部受支持的参数组合。
    pub formats: Vec<FormatRange>,
    /// 枚举这台设备时出的错。
    ///
    /// **出错的设备仍然留在列表里**，只是带一个理由——直接过滤掉的话，
    /// 用户看到的是「我的麦克风不见了」，而真实原因是它正被别的程序
    /// 独占。把理由带出来，用户才知道该干什么。
    pub error: Option<String>,
}

/// 一次枚举的全部结果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCatalog {
    /// 后端名字（Windows 上是 `WASAPI`，macOS 上是 `CoreAudio`）。
    pub host: String,
    pub inputs: Vec<DeviceInfo>,
    pub outputs: Vec<DeviceInfo>,
}

impl DeviceCatalog {
    pub fn find_input(&self, id: &str) -> Option<&DeviceInfo> {
        self.inputs.iter().find(|d| d.id == id)
    }

    pub fn find_output(&self, id: &str) -> Option<&DeviceInfo> {
        self.outputs.iter().find(|d| d.id == id)
    }
}

/// 后端名字。
///
/// 用 `cfg!` 而不是问 cpal：cpal 0.18 的 `HostTrait` 没有暴露 `id()`，
/// 而 `Host` 的 `Debug` 输出是不保证稳定的内部形式。这里只是给人看的
/// 一个标签，`cfg!` 够了——写错也不会有任何东西依赖它。
pub fn host_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "WASAPI"
    } else if cfg!(target_os = "macos") {
        "CoreAudio"
    } else if cfg!(target_os = "linux") {
        "ALSA"
    } else {
        "unknown"
    }
}

// ---------------------------------------------------------------------------
// 枚举
// ---------------------------------------------------------------------------

/// 列出所有输入和输出设备。
///
/// 在**命令线程**上跑，不在实时路径上——它会做一堆系统调用，可能耗时
/// 几百毫秒（WASAPI 尤其慢）。UI 上要有「正在枚举」的状态。
pub fn enumerate() -> Result<DeviceCatalog, CoreError> {
    let host = cpal::default_host();

    let default_input_id = host.default_input_device().and_then(|d| d.id().ok());
    let default_output_id = host.default_output_device().and_then(|d| d.id().ok());

    let inputs = collect(host.input_devices(), default_input_id.as_ref(), true)?;
    let outputs = collect(host.output_devices(), default_output_id.as_ref(), false)?;

    Ok(DeviceCatalog {
        host: host_name().to_string(),
        inputs,
        outputs,
    })
}

/// 把一台设备枚举成 [`DeviceInfo`]。
///
/// 抽出来是为了让 `collect` 不用同时管两个方向——输入和输出在 cpal 里
/// 是两套不同的迭代器类型，除了 `is_input` 之外逐行相同。
fn collect(
    devices: Result<impl Iterator<Item = Device>, cpal::Error>,
    default_id: Option<&DeviceId>,
    is_input: bool,
) -> Result<Vec<DeviceInfo>, CoreError> {
    let devices = devices.map_err(|e| CoreError::DeviceUnavailable {
        device: if is_input { "输入设备列表" } else { "输出设备列表" }.into(),
        reason: describe_cpal_error(&e),
    })?;

    Ok(devices
        .map(|d| describe_device(&d, default_id, is_input))
        .collect())
}

fn describe_device(device: &Device, default_id: Option<&DeviceId>, is_input: bool) -> DeviceInfo {
    let id = device.id().map(|i| i.to_string()).unwrap_or_default();
    let name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| id.clone());

    let is_default = default_id.is_some_and(|d| d.to_string() == id);

    // 默认参数和设备能力是两次独立调用，各自可能失败。分开收集，
    // 这样「能列出能力但拿不到默认值」这种半残状态也如实反映出来。
    let default_result = if is_input {
        device.default_input_config()
    } else {
        device.default_output_config()
    };
    let (default_format, mut error) = match default_result {
        Ok(c) => (Some(format_range_from_config(&c)), None),
        Err(e) => (None, Some(describe_cpal_error(&e))),
    };

    // 输入和输出走不同的分支而不是合并：`SupportedInputConfigs` 和
    // `SupportedOutputConfigs` 在 cpal 里是两个不同的类型，合不进
    // 一个 `if`。两次调用各自收集，半残状态如实反映。
    let mut formats = Vec::new();
    if is_input {
        match device.supported_input_configs() {
            Ok(ranges) => formats.extend(ranges.map(|r| format_range_from_supported(&r))),
            Err(e) => {
                error.get_or_insert(describe_cpal_error(&e));
            }
        }
    } else {
        match device.supported_output_configs() {
            Ok(ranges) => formats.extend(ranges.map(|r| format_range_from_supported(&r))),
            Err(e) => {
                error.get_or_insert(describe_cpal_error(&e));
            }
        }
    }

    DeviceInfo {
        id,
        name,
        is_default,
        default_format,
        formats,
        error,
    }
}

fn format_range_from_config(c: &SupportedStreamConfig) -> FormatRange {
    let code = format_code(c.sample_format());
    let rate = c.sample_rate();
    FormatRange {
        channels: c.channels(),
        min_sample_rate: rate,
        max_sample_rate: rate,
        format: format_name(code).to_string(),
        format_code: code,
        bits: format_bits(c.sample_format()),
        buffer: buffer_range(c.buffer_size()),
    }
}

fn format_range_from_supported(r: &SupportedStreamConfigRange) -> FormatRange {
    let code = format_code(r.sample_format());
    FormatRange {
        channels: r.channels(),
        min_sample_rate: r.min_sample_rate(),
        max_sample_rate: r.max_sample_rate(),
        format: format_name(code).to_string(),
        format_code: code,
        bits: format_bits(r.sample_format()),
        buffer: buffer_range(r.buffer_size()),
    }
}

fn buffer_range(b: &SupportedBufferSize) -> BufferRange {
    match *b {
        SupportedBufferSize::Range { min, max } => BufferRange::Range { min, max },
        SupportedBufferSize::Unknown => BufferRange::Unknown,
    }
}

/// 把 cpal 的错误压成一句能显示给用户的话。
///
/// **带上 `kind` 的 `Debug` 名**：`message()` 在多数后端上是空的，
/// 只显示 message 的话用户看到的是「设备错误」四个字，而
/// `DeviceBusy`（别的程序占着）和 `DeviceNotAvailable`（拔了）需要
/// 完全不同的补救动作。
pub fn describe_cpal_error(e: &cpal::Error) -> String {
    match e.message() {
        Some(m) if !m.is_empty() => format!("{:?}：{m}", e.kind()),
        _ => format!("{:?}", e.kind()),
    }
}

// ---------------------------------------------------------------------------
// 错误回调的落点
// ---------------------------------------------------------------------------

/// 送出与丢弃的计数。
#[derive(Debug, Default)]
pub struct ErrorCounters {
    sent: AtomicU64,
    dropped: AtomicU64,
}

impl ErrorCounters {
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// 有多少条错误**被丢掉了**。
    ///
    /// 这个计数器非零必须报给 UI：「设备一直在报错但一条都没送到」和
    /// 「设备一直正常」在界面上长得一模一样，而这两件事的排查方向
    /// 差着十万八千里。
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// 把 cpal 的错误回调接到非实时线程上。
///
/// # 为什么是「有界 + try_send」而不是无界通道
///
/// 错误回调**可能被实时线程调用**，所以这里不能阻塞。`bounded` 通道的
/// `try_send` 走的是预分配的数组队列，既不加锁也不分配——和两个数据
/// 回调遵守同一条纪律。
///
/// 无界通道看起来更省事（错误那么少，怎么会满），但它在**消费端已经
/// 死掉**时会无限增长，而错误回调恰恰是唯一一条我们无法保证消费端
/// 还活着的路径。真到那一步，用户看到的是程序被一个错误日志吃光内存。
///
/// 所以满了就丢，并且**丢的条数要能被查**（[`ErrorCounters::dropped`]）。
#[derive(Clone, Debug)]
pub struct ErrorSink {
    tx: Sender<cpal::Error>,
    counters: Arc<ErrorCounters>,
}

impl ErrorSink {
    /// 返回（写入端，读取端，计数）。
    ///
    /// 计数单独返回一个句柄是因为引擎要能在**不接收**的情况下看丢弃数。
    pub fn new() -> (Self, Receiver<cpal::Error>, Arc<ErrorCounters>) {
        let (tx, rx) = bounded(ERROR_CHANNEL_CAPACITY);
        let counters = Arc::new(ErrorCounters::default());
        (
            Self {
                tx,
                counters: Arc::clone(&counters),
            },
            rx,
            counters,
        )
    }

    /// 从错误回调里调用。不阻塞、不分配。
    pub fn report(&self, e: cpal::Error) {
        match self.tx.try_send(e) {
            Ok(()) => {
                self.counters.sent.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// 错误通道容量。远超正常人会看到的条数，小到不会被一次错误风暴吃光。
pub const ERROR_CHANNEL_CAPACITY: usize = 64;

// ---------------------------------------------------------------------------
// 打开流之前的协商
// ---------------------------------------------------------------------------

/// 设备的显示名，拿不到就给一个诚实的占位符。
///
/// 用在错误信息里：「无法使用设备：输出流建立失败」和「无法使用设备
/// 扬声器 (Realtek High Definition Audio)：输出流建立失败」的可用性
/// 差很多，而用户报告问题时我们能问的第一个问题就是「哪个设备」。
///
/// `description()` 在多数后端上会分配一个 `String`——调用方都在建流
/// 这条非实时路径上，所以无所谓。
pub fn device_name(device: &Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "未知设备".into())
}

/// 按设备 id 找输入设备。
///
/// `id` 为空表示「用系统默认」——这是首次启动时的正常情况，不是错误。
/// 用户在配置里存下来的 id 是**跨程序运行稳定**的（cpal 的 `DeviceId`
/// 契约），但设备被拔掉之后它就不再解析得出来，那时要报
/// [`CoreError::DeviceNotFound`] 并让引擎退回默认设备重试。
pub fn find_input(id: &str) -> Result<Device, CoreError> {
    let host = cpal::default_host();
    if id.is_empty() {
        return host
            .default_input_device()
            .ok_or_else(|| CoreError::DeviceNotFound("系统默认输入设备".into()));
    }
    let want = DeviceId::from_str(id).map_err(|e| CoreError::DeviceNotFound(format!("{id}：{e}")))?;
    host.device_by_id(&want).ok_or_else(|| CoreError::DeviceNotFound(id.to_string()))
}

/// 按设备 id 找输出设备。语义同 [`find_input`]。
pub fn find_output(id: &str) -> Result<Device, CoreError> {
    let host = cpal::default_host();
    if id.is_empty() {
        return host
            .default_output_device()
            .ok_or_else(|| CoreError::DeviceNotFound("系统默认输出设备".into()));
    }
    let want = DeviceId::from_str(id).map_err(|e| CoreError::DeviceNotFound(format!("{id}：{e}")))?;
    host.device_by_id(&want).ok_or_else(|| CoreError::DeviceNotFound(id.to_string()))
}

/// 打开流用的全部参数，以及协商过程中产生的、需要告诉用户的偏差。
#[derive(Clone, Debug)]
pub struct Negotiated {
    /// 交给 `build_*_stream` 的参数。
    pub config: StreamConfig,
    /// 设备承诺支持的格式，由它决定用哪个泛型去建流。
    pub format: SampleFormat,
    /// 设备报告的可接受缓冲范围。
    pub supported_buffer: BufferRange,
    /// 协商中被降级的地方。空表示请求全部得到了满足。
    ///
    /// **任何非空都要出现在 UI 上**。理由和采样率下限一样：静默降级
    /// 比降级本身更糟——用户以为自己在看 48 kHz 的证据，实际是 44.1 kHz。
    pub notes: Vec<String>,
}

/// 从设备的默认参数出发，协商出一份可用的流参数。
///
/// # 为什么从 `default_*_config` 出发而不是从 48 kHz 出发
///
/// 因为那是**操作系统认为正确的参数**。WASAPI 共享模式下，请求一个
/// 设备不原生的采样率会让系统插一个重采样器——旧版 Windows 的那个
/// 重采样器会连带施加高通，把整个 20–50 Hz 段削掉，而我们在频谱上
/// 看到的将是「麦克风没有低频」，排查方向会完全跑偏。
///
/// 请求用户偏好的采样率只在**设备明确支持**时才发出去（[`FormatRange::pick_rate`]）。
pub fn negotiate(
    device: &Device,
    is_input: bool,
    preferred_rate: Option<u32>,
    preferred_buffer: Option<FrameCount>,
) -> Result<Negotiated, CoreError> {
    let name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "未知设备".into());

    let default_cfg = if is_input {
        device.default_input_config()
    } else {
        device.default_output_config()
    }
    .map_err(|e| CoreError::DeviceUnavailable {
        device: name.clone(),
        reason: describe_cpal_error(&e),
    })?;

    let mut notes = Vec::new();

    // 用户偏好的采样率：设备支持就换，不支持就说清楚（而不是悄悄忽略）。
    let chosen = match preferred_rate {
        None => default_cfg,
        Some(rate) => {
            if rate == default_cfg.sample_rate() {
                default_cfg
            } else {
                match pick_supported(device, is_input, rate, default_cfg.channels()) {
                    Some(c) => {
                        notes.push(format!(
                            "已按请求把采样率改为 {rate} Hz（设备默认 {} Hz）",
                            default_cfg.sample_rate()
                        ));
                        c
                    }
                    None => {
                        notes.push(format!(
                            "设备不支持 {rate} Hz，改用默认的 {} Hz",
                            default_cfg.sample_rate()
                        ));
                        default_cfg
                    }
                }
            }
        }
    };

    let rate = chosen.sample_rate();
    if rate < MIN_SAMPLE_RATE {
        return Err(CoreError::SampleRateTooLow {
            actual: rate,
            min: MIN_SAMPLE_RATE,
        });
    }

    let supported_buffer = buffer_range(chosen.buffer_size());
    let buffer_size = match preferred_buffer {
        None => BufferSize::Default,
        Some(frames) => match supported_buffer {
            BufferRange::Range { min, max } if min <= frames && frames <= max => {
                BufferSize::Fixed(frames)
            }
            BufferRange::Unknown => {
                // 后端不肯说范围。**照发不误**：CoreAudio 和 WASAPI 都
                // 落在这里，而它们都能接受固定缓冲；真的不接受的话
                // `build_*_stream` 会报错，那是个明确的失败，比我们自己
                // 猜范围然后拒绝要好。
                BufferSize::Fixed(frames)
            }
            BufferRange::Range { min, max } => {
                notes.push(format!(
                    "设备的缓冲范围是 {min}–{max} 帧，装不下请求的 {frames} 帧；\
                     改用后端默认值"
                ));
                BufferSize::Default
            }
        },
    };

    let channels = chosen.channels();
    if channels == 0 {
        return Err(CoreError::DeviceUnavailable {
            device: name,
            reason: "设备报告了 0 个声道".into(),
        });
    }

    Ok(Negotiated {
        config: StreamConfig {
            channels,
            sample_rate: rate,
            buffer_size,
        },
        format: chosen.sample_format(),
        supported_buffer,
        notes,
    })
}

/// 在设备的能力列表里找一个「给定采样率 + 给定声道数」的配置。
///
/// 优先返回**与设备默认格式相同的采样格式**：换采样格式意味着换一个
/// 建流时用的泛型，而格式一变，`convert.rs` 的归一化路径也就变了。
/// 能在同一个格式里解决的事不要跨格式解决。
fn pick_supported(
    device: &Device,
    is_input: bool,
    rate: u32,
    channels: u16,
) -> Option<SupportedStreamConfig> {
    if rate < MIN_SAMPLE_RATE {
        return None;
    }

    // 同样因为两个迭代器类型不同，先各自收进同一只 `Vec` 再统一筛选。
    let mut ranges = Vec::new();
    if is_input {
        ranges.extend(device.supported_input_configs().ok()?);
    } else {
        ranges.extend(device.supported_output_configs().ok()?);
    }

    let mut fallback = None;
    for r in ranges {
        if r.channels() != channels || !r.contains_rate(rate) {
            continue;
        }
        // DSD 能被列出但打不开（见 `is_capturable`）。不挡掉的话，
        // 这里会高高兴兴地返回一个 `build_*_stream` 必然失败的配置，
        // 而错误信息会指向 cpal 内部，跟真正的原因差着十万八千里。
        if !is_capturable(r.sample_format()) {
            continue;
        }
        // 记下第一个能用的，作为「没有 f32 就用它」的兜底。
        if fallback.is_none() {
            fallback = r.try_with_sample_rate(rate);
        }
        if r.sample_format() == SampleFormat::F32 {
            return r.try_with_sample_rate(rate);
        }
    }
    fallback
}

// ---------------------------------------------------------------------------
// 给 UI 看的流参数
// ---------------------------------------------------------------------------

/// 一条流**实际**跑在什么参数上。
///
/// 这是 Phase 1 验证清单里明确要求显示到 UI 上的东西——不是我们请求的，
/// 是达成的。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: String,
    pub format_code: u8,
    pub bits: u32,
    /// 固定缓冲的帧数；`None` 表示由后端决定（读不回来）。
    pub buffer_frames: Option<u32>,
}

impl StreamInfo {
    pub fn new(cfg: &StreamConfig, format: SampleFormat) -> Self {
        let code = format_code(format);
        Self {
            sample_rate: cfg.sample_rate,
            channels: cfg.channels,
            format: format_name(code).to_string(),
            format_code: code,
            bits: format_bits(format),
            buffer_frames: match cfg.buffer_size {
                BufferSize::Fixed(n) => Some(n),
                BufferSize::Default => None,
            },
        }
    }

    /// 一行给人看的描述，UI 的状态栏直接用。
    pub fn summary(&self) -> String {
        let buf = match self.buffer_frames {
            Some(n) => format!("{n} 帧"),
            None => "后端默认".to_string(),
        };
        format!(
            "{} Hz / {} 声道 / {} / 缓冲 {buf}",
            self.sample_rate, self.channels, self.format
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_formats() -> [SampleFormat; 15] {
        [
            SampleFormat::I8,
            SampleFormat::I16,
            SampleFormat::I24,
            SampleFormat::I32,
            SampleFormat::I64,
            SampleFormat::U8,
            SampleFormat::U16,
            SampleFormat::U24,
            SampleFormat::U32,
            SampleFormat::U64,
            SampleFormat::F32,
            SampleFormat::F64,
            SampleFormat::DsdU8,
            SampleFormat::DsdU16,
            SampleFormat::DsdU32,
        ]
    }

    /// 编号是有线格式：写进 session manifest，也可能被将来的版本读回来。
    /// 所以每个变体都要一一对上，且**往返无损**。
    #[test]
    fn format_codes_round_trip() {
        let mut seen = std::collections::BTreeSet::new();
        for f in all_formats() {
            let code = format_code(f);
            assert!(seen.insert(code), "{f:?} 的编号 {code} 与别的格式撞了");
            assert_eq!(format_from_code(code), Some(f), "{f:?} 往返失败");
            assert_ne!(format_name(code), "unknown", "{f:?} 没有名字");
        }
        assert_eq!(seen.len(), all_formats().len(), "每种格式应给出不同编号");
    }

    /// **DSD 会被设备列出来，但我们打不开。**
    ///
    /// `dasp_sample::Sample` 没有为 DSD 实现，带类型的
    /// `build_input_stream::<T, _>` 实例化不出来。不显式挡掉的话，
    /// 协商会返回一个必然失败的配置，而错误会指向 cpal 内部，
    /// 跟真正的原因（这是 1 bit 位流，不是数值样本）差着十万八千里。
    #[test]
    fn dsd_formats_are_listed_but_not_capturable() {
        for f in [SampleFormat::DsdU8, SampleFormat::DsdU16, SampleFormat::DsdU32] {
            assert!(!is_capturable(f), "{f:?} 不该被认为是可打开的");
            // 但它们仍然要有编号和名字——否则日志里会显示成「未知格式」，
            // 而用户会以为设备报了个垃圾值。
            assert_ne!(format_name(format_code(f)), "unknown");
        }
        for f in [
            SampleFormat::I8,
            SampleFormat::I16,
            SampleFormat::I24,
            SampleFormat::I32,
            SampleFormat::I64,
            SampleFormat::U8,
            SampleFormat::U16,
            SampleFormat::U24,
            SampleFormat::U32,
            SampleFormat::U64,
            SampleFormat::F32,
            SampleFormat::F64,
        ] {
            assert!(is_capturable(f), "{f:?} 应该能打开");
        }
    }

    /// 未知编号不能被猜成一个真实格式。来自未来版本的 manifest 被当成
    /// f32 读进来，会让所有录音的电平差得很远，而且是静默的。
    #[test]
    fn unknown_format_code_is_not_guessed() {
        assert_eq!(format_from_code(15), None);
        assert_eq!(format_from_code(255), None);
        assert_eq!(format_name(200), "unknown");
        assert_eq!(FORMAT_CODE_UNKNOWN, 255);
        // 兜底编号本身不能被解析回一个真实格式
        assert_eq!(format_from_code(FORMAT_CODE_UNKNOWN), None);
    }

    /// i24 是 4 字节存储、24 位有效。这两个数搞混了，读 WAV 头
    /// 或者算量化噪声都会错。
    #[test]
    fn i24_reports_24_significant_bits() {
        assert_eq!(format_bits(SampleFormat::I24), 24);
        assert_eq!(format_bits(SampleFormat::U24), 24);
        assert_eq!(format_bits(SampleFormat::I32), 32);
        assert_eq!(format_bits(SampleFormat::F32), 32);
    }

    fn range(min: u32, max: u32) -> FormatRange {
        FormatRange {
            channels: 2,
            min_sample_rate: min,
            max_sample_rate: max,
            format: "f32".into(),
            format_code: 10,
            bits: 32,
            buffer: BufferRange::Unknown,
        }
    }

    /// 偏好速率在范围内就用它。
    #[test]
    fn pick_rate_prefers_the_request() {
        assert_eq!(range(44_100, 96_000).pick_rate(Some(48_000)), Some(48_000));
    }

    /// 偏好速率超出范围时退到最高值，不是最低值——高端通常是设备原生
    /// 支持的速率，低端往往是重采样出来的。
    #[test]
    fn pick_rate_falls_back_to_the_top_of_the_range() {
        assert_eq!(range(44_100, 96_000).pick_rate(Some(192_000)), Some(96_000));
        assert_eq!(range(44_100, 96_000).pick_rate(None), Some(96_000));
    }

    /// **整个范围都在下限之下时必须返回 `None`。**
    ///
    /// 这正是 macOS 切到蓝牙 HFP 通话模式的情形：8 kHz 窄带。返回一个
    /// "可用"的 8 kHz 配置会让系统带着一个 A 计权滤波器已经失真的状态
    /// 跑起来，而那是最难查的一类问题——一切看起来都在工作。
    #[test]
    fn pick_rate_refuses_a_range_below_the_minimum() {
        assert_eq!(range(8_000, 8_000).pick_rate(None), None);
        assert_eq!(range(8_000, 11_025).pick_rate(Some(8_000)), None);
    }

    /// 顶端正好落在下限上时**能用**——16 kHz 是下限而不是禁区，
    /// 语音优先的蓝牙耳机在 HFP 下给出的往往正是这一档。
    #[test]
    fn a_range_topping_out_at_the_floor_is_still_usable() {
        // 偏好的 8 kHz 不可用，退到范围内可用的最高值 16 kHz
        assert_eq!(range(8_000, 16_000).pick_rate(Some(8_000)), Some(16_000));
        // 范围跨过下限：同样是退到顶端
        assert_eq!(range(8_000, 44_100).pick_rate(Some(8_000)), Some(44_100));
    }

    /// `usable_at` 是下限和范围的合取，两条都不能少。
    #[test]
    fn usable_at_requires_both_the_range_and_the_floor() {
        let r = range(8_000, 48_000);
        assert!(r.usable_at(44_100));
        assert!(r.usable_at(16_000), "下限本身是允许的");
        assert!(!r.usable_at(15_999), "低于下限一格就不行");
        assert!(!r.usable_at(8_000), "在范围内但低于下限");
        assert!(!r.usable_at(96_000), "高于范围顶端");
    }

    /// 低于下限的采样率必须是**拒绝**，不是降级。
    #[test]
    fn min_sample_rate_is_sixteen_kilohertz() {
        assert_eq!(MIN_SAMPLE_RATE, 16_000);
        // 44.1k 和 48k 当然都过
        assert!(MIN_SAMPLE_RATE <= 44_100);
    }

    #[test]
    fn stream_info_summarizes_what_was_achieved() {
        let cfg = StreamConfig {
            channels: 2,
            sample_rate: 44_100,
            buffer_size: BufferSize::Default,
        };
        let info = StreamInfo::new(&cfg, SampleFormat::I16);
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 2);
        assert_eq!(info.format, "i16");
        assert_eq!(info.buffer_frames, None);
        assert!(info.summary().contains("44100"), "{}", info.summary());
        assert!(info.summary().contains("后端默认"), "{}", info.summary());
    }

    #[test]
    fn fixed_buffer_shows_its_size() {
        let cfg = StreamConfig {
            channels: 1,
            sample_rate: 48_000,
            buffer_size: BufferSize::Fixed(480),
        };
        let info = StreamInfo::new(&cfg, SampleFormat::F32);
        assert_eq!(info.buffer_frames, Some(480));
        assert!(info.summary().contains("480 帧"), "{}", info.summary());
    }

    /// 后端名字只是给人看的标签，但至少要对得上平台——
    /// 写错了会让「Windows 上跑的是 WASAPI 还是 ASIO」变成一个
    /// 需要读代码才能回答的问题。
    #[test]
    fn host_name_matches_the_platform() {
        let h = host_name();
        if cfg!(target_os = "windows") {
            assert_eq!(h, "WASAPI");
        } else if cfg!(target_os = "macos") {
            assert_eq!(h, "CoreAudio");
        }
        assert_ne!(h, "");
    }
}
