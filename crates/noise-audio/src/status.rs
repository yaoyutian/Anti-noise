//! 后台线程写、UI 读的状态。
//!
//! # 两种快照，因为它们的读取频率差两个数量级
//!
//! * [`MonitorStatus`] —— **全是原子标量**，不取锁。状态栏、计数器、
//!   流参数走这里。它每秒被读几次，而且要在引擎线程和命令线程之间共享，
//!   所以宁可多写几十行存取方法，也不要一个 `Mutex`。
//! * [`LevelSnapshot`] —— 带数组（八度带 6 个值、阈值、若干 dB），
//!   没法原子化，用一个 `Mutex` 包住。它**只被 20 Hz 的发射器**读，
//!   而且读的线程不是实时线程，加锁完全可以。
//!
//! 两者都**不由实时回调触碰**。回调只碰环和它自己持有的原子量。
//!
//! # 为什么全用 `Relaxed`
//!
//! 这些字段互不相干——`frames_in` 和 `events_started` 之间没有任何
//! 「一并可见」的契约需要维持，UI 上偶尔看到差一帧的两个数字完全无害。
//! 用 `SeqCst` 只会让每次计数都插一条内存屏障，换来的是一份**并不存在**
//! 的一致性保证：真正的时序一致性在 UI 那边靠「读两次、两次都当近似值」
//! 就已经够了。唯一的例外是 [`MonitorStatus::take_run_change`]，
//! 它要的是一个边沿信号，那条用 `AcqRel`。

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use noise_detect::{DetectorState, GateState};
use serde::{Deserialize, Serialize};

use crate::device::StreamInfo;

/// 监测的整体运行状态。
///
/// 与 [`DetectorState`] 是两回事：那个说的是「有没有事件在进行」，
/// 这个说的是「我们有没有在跑」。UI 上两个都要显示——「正在监测，
/// 当前空闲」和「已停止」是完全不同的两句话。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// 没有在跑。启动前的初始状态，以及用户点了停止之后。
    Stopped,
    /// 正在打开设备、建流。**这一步可能耗时几百毫秒**（WASAPI 尤其慢），
    /// UI 必须显示它，否则用户会以为按钮没反应而再点一次。
    Starting,
    /// 正常监测中。
    Running,
    /// 还在跑，但某些东西不对——见 [`degraded`]。**继续跑而不是退出**：
    /// 一个降级运行的监测器仍然能录下证据，而一个退出的监测器什么也录不到。
    Degraded,
    /// 正在收尾（关闭流、finalize 录音）。
    Stopping,
}

impl RunState {
    fn to_u8(self) -> u8 {
        match self {
            Self::Stopped => 0,
            Self::Starting => 1,
            Self::Running => 2,
            Self::Degraded => 3,
            Self::Stopping => 4,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Starting,
            2 => Self::Running,
            3 => Self::Degraded,
            4 => Self::Stopping,
            _ => Self::Stopped,
        }
    }

    /// 是否在跑（含降级）。
    pub fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Degraded)
    }
}

/// 降级的种类。用位图而不是枚举，因为**同时**降级是常态
/// （拔了麦克风又赶上录音盘满，两件事都会发生）。
pub mod degraded {
    /// 设备不支持的采样率被换成了别的——实际跑的不是配置里写的那一个。
    pub const SAMPLE_RATE_CLAMPED: u64 = 1 << 0;
    /// 某个滤波器的设计频率高于 Nyquist 被钳位，那一带的读数不可信。
    pub const DSP_FILTER_CLAMPED: u64 = 1 << 1;
    /// 丢过音频块。**已经发生的故障的表征**，不是故障本身。
    pub const DROPPED_AUDIO: u64 = 1 << 2;
    /// 实时回调里发生过内存分配。只可能出现在开了 `rt-guard` 的构建里。
    pub const RT_VIOLATION: u64 = 1 << 3;
    /// L4 速率看门狗触发，回放已被自动停掉。
    pub const RUNAWAY_SUSPENDED: u64 = 1 << 4;
    /// 流断过并被重启过（插拔设备、被别的程序抢走）。
    pub const DEVICE_RESTARTED: u64 = 1 << 5;
    /// 录音不完整——事件有了但音频缺段。
    ///
    /// **这一条对取证最要命**：日志说 21:04 有一次冲击，而那一分钟
    /// 的 WAV 是空的。宁可让报告显式标注「该事件无录音」，
    /// 也不要让用户拿着一段静音去投诉。
    pub const EVIDENCE_INCOMPLETE: u64 = 1 << 6;
    /// 没有做过校准，因此所有绝对 dB SPL 的主张都不成立。
    pub const UNCALIBRATED: u64 = 1 << 7;
}

/// 位图 → 一句句人话。UI 直接显示这个列表。
pub fn describe_degraded(flags: u64) -> Vec<&'static str> {
    use degraded as d;
    let mut out = Vec::new();
    if flags & d::SAMPLE_RATE_CLAMPED != 0 {
        out.push("实际采样率不是配置里指定的那一个");
    }
    if flags & d::DSP_FILTER_CLAMPED != 0 {
        out.push("滤波器设计频率被采样率钳位，低频读数不可信");
    }
    if flags & d::DROPPED_AUDIO != 0 {
        out.push("有音频块被丢弃，录音可能有缺口");
    }
    if flags & d::RT_VIOLATION != 0 {
        out.push("实时回调内发生过内存分配，可能出爆音");
    }
    if flags & d::RUNAWAY_SUSPENDED != 0 {
        out.push("回放速率超限，已自动停止回放");
    }
    if flags & d::DEVICE_RESTARTED != 0 {
        out.push("音频设备断过并已重启");
    }
    if flags & d::EVIDENCE_INCOMPLETE != 0 {
        out.push("部分事件没有完整录音");
    }
    if flags & d::UNCALIBRATED != 0 {
        out.push("未校准：只能显示 dBFS，不能主张绝对声压级");
    }
    out
}

/// f64 塞进原子的位模式。
///
/// 用 `f32` 而不是 `f64`：这里存的全是**要显示的数**，不是要再参与
/// 计算的中间量。f32 有 7 位有效数字，而 dB 值显示到小数点后一位就
/// 足够了。省下的一半空间换成一次更少的缓存行占用，是划算的。
#[inline]
fn pack(v: f32) -> u32 {
    v.to_bits()
}

#[inline]
fn unpack(v: u32) -> f32 {
    f32::from_bits(v)
}

/// 一个数值的上限哨兵——`f32::NAN` 在原子量里是合法的位模式，
/// 但读出来之后再算就是灾难。写入侧统一用这个函数把非有限值挡掉。
#[inline]
fn pack_finite(v: f64) -> u32 {
    let v = v as f32;
    pack(if v.is_finite() { v } else { noise_detect::DB_FLOOR as f32 })
}

/// 后台线程写、任意线程读的原子状态。
#[derive(Debug)]
pub struct MonitorStatus {
    run: AtomicU8,
    /// 上一次被读走的 `run` 值，用于产生「状态变了」的边沿信号。
    run_seen: AtomicU8,
    detector: AtomicU8,
    gate: AtomicU8,

    // ---- 实际达成的流参数 ----
    // 分开存而不是存一个 Option<StreamInfo>：原子量里放不下结构体，
    // 而「有多少个开放位」这个判据必须是一个单独的原子量——用
    // sample_rate == 0 当哨兵的话，一个真的报了 0 的设备（驱动 bug）
    // 会被当成「没打开」。
    input_open: AtomicBool,
    input_sample_rate: AtomicU32,
    input_channels: AtomicU16,
    input_format: AtomicU8,
    output_open: AtomicBool,
    output_sample_rate: AtomicU32,
    output_channels: AtomicU16,
    output_format: AtomicU8,

    // ---- 计数器 ----
    frames_in: AtomicU64,
    frames_out: AtomicU64,
    dropped_in: AtomicU64,
    dropped_out: AtomicU64,
    dropped_blocks: AtomicU64,
    events_started: AtomicU64,
    impacts: AtomicU64,
    candidates_rejected: AtomicU64,
    suppressed_by_gate: AtomicU64,
    rt_violations: AtomicU64,
    degraded: AtomicU64,

    // ---- 回放 ----
    playback_active: AtomicBool,
    playback_gain_db: AtomicU32,

    // ---- 最新电平 ----
    level_dbfs: AtomicU32,
    floor_dbfs: AtomicU32,
    /// 会话开始以来的毫秒数，随最新的分析帧更新。
    t_ms: AtomicU32,
}

impl Default for MonitorStatus {
    fn default() -> Self {
        Self::new()
    }
}

impl MonitorStatus {
    pub fn new() -> Self {
        Self {
            run: AtomicU8::new(RunState::Stopped.to_u8()),
            run_seen: AtomicU8::new(RunState::Stopped.to_u8()),
            detector: AtomicU8::new(detector_to_u8(DetectorState::Idle)),
            gate: AtomicU8::new(gate_to_u8(GateState::Open)),
            input_open: AtomicBool::new(false),
            input_sample_rate: AtomicU32::new(0),
            input_channels: AtomicU16::new(0),
            input_format: AtomicU8::new(0),
            output_open: AtomicBool::new(false),
            output_sample_rate: AtomicU32::new(0),
            output_channels: AtomicU16::new(0),
            output_format: AtomicU8::new(0),
            frames_in: AtomicU64::new(0),
            frames_out: AtomicU64::new(0),
            dropped_in: AtomicU64::new(0),
            dropped_out: AtomicU64::new(0),
            dropped_blocks: AtomicU64::new(0),
            events_started: AtomicU64::new(0),
            impacts: AtomicU64::new(0),
            candidates_rejected: AtomicU64::new(0),
            suppressed_by_gate: AtomicU64::new(0),
            rt_violations: AtomicU64::new(0),
            degraded: AtomicU64::new(0),
            playback_active: AtomicBool::new(false),
            playback_gain_db: AtomicU32::new(pack(0.0)),
            level_dbfs: AtomicU32::new(pack_finite(noise_detect::DB_FLOOR)),
            floor_dbfs: AtomicU32::new(pack_finite(noise_detect::DB_FLOOR)),
            t_ms: AtomicU32::new(0),
        }
    }

    // ---- 运行状态 ----

    pub fn set_run(&self, s: RunState) {
        self.run.store(s.to_u8(), Ordering::Relaxed);
    }

    pub fn run(&self) -> RunState {
        RunState::from_u8(self.run.load(Ordering::Relaxed))
    }

    /// 状态**变了**就返回新值，没变返回 `None`。
    ///
    /// 边沿触发，给发射器用：状态栏事件只在变化时推，不是 20 Hz 推。
    /// 用 `AcqRel` 而不是 `Relaxed`——这里要的恰恰是「只有一个读者
    /// 拿到这一次变化」，那是 `compare_exchange` 的语义。
    pub fn take_run_change(&self) -> Option<RunState> {
        let now = self.run.load(Ordering::Acquire);
        loop {
            let seen = self.run_seen.load(Ordering::Relaxed);
            if seen == now {
                return None;
            }
            if self
                .run_seen
                .compare_exchange(seen, now, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(RunState::from_u8(now));
            }
        }
    }

    pub fn set_detector(&self, s: DetectorState) {
        self.detector.store(detector_to_u8(s), Ordering::Relaxed);
    }

    pub fn detector(&self) -> DetectorState {
        detector_from_u8(self.detector.load(Ordering::Relaxed))
    }

    pub fn set_gate(&self, s: GateState) {
        self.gate.store(gate_to_u8(s), Ordering::Relaxed);
    }

    pub fn gate(&self) -> GateState {
        gate_from_u8(self.gate.load(Ordering::Relaxed))
    }

    // ---- 流参数 ----

    pub fn set_input(&self, info: &StreamInfo) {
        self.input_sample_rate.store(info.sample_rate, Ordering::Relaxed);
        self.input_channels.store(info.channels, Ordering::Relaxed);
        self.input_format.store(info.format_code, Ordering::Relaxed);
        // `open` 最后置位：先填好参数再宣布打开，这样读者不可能看到
        // 「打开了但采样率是 0」这种半截状态。
        self.input_open.store(true, Ordering::Release);
    }

    pub fn clear_input(&self) {
        self.input_open.store(false, Ordering::Release);
    }

    /// 输入流的实际参数。`None` 表示没在跑。
    pub fn input(&self) -> Option<StreamInfo> {
        if !self.input_open.load(Ordering::Acquire) {
            return None;
        }
        Some(self.build_info(
            self.input_sample_rate.load(Ordering::Relaxed),
            self.input_channels.load(Ordering::Relaxed),
            self.input_format.load(Ordering::Relaxed),
        ))
    }

    pub fn set_output(&self, info: &StreamInfo) {
        self.output_sample_rate.store(info.sample_rate, Ordering::Relaxed);
        self.output_channels.store(info.channels, Ordering::Relaxed);
        self.output_format.store(info.format_code, Ordering::Relaxed);
        self.output_open.store(true, Ordering::Release);
    }

    pub fn clear_output(&self) {
        self.output_open.store(false, Ordering::Release);
    }

    pub fn output(&self) -> Option<StreamInfo> {
        if !self.output_open.load(Ordering::Acquire) {
            return None;
        }
        Some(self.build_info(
            self.output_sample_rate.load(Ordering::Relaxed),
            self.output_channels.load(Ordering::Relaxed),
            self.output_format.load(Ordering::Relaxed),
        ))
    }

    fn build_info(&self, sample_rate: u32, channels: u16, format_code: u8) -> StreamInfo {
        StreamInfo {
            sample_rate,
            channels,
            format: crate::device::format_name(format_code).to_string(),
            format_code,
            bits: crate::device::format_from_code(format_code)
                .map(crate::device::format_bits)
                .unwrap_or(0),
            // 缓冲大小是唯一一个我们真的拿不回来、只能记下请求值的字段。
            // 存成 `None` 而不是编一个数字——UI 上显示「后端默认」是诚实的，
            // 显示「480 帧」而实际是 960 是在撒谎。
            buffer_frames: None,
        }
    }

    // ---- 计数器 ----

    /// 写入**绝对值**，不是增量。
    ///
    /// 环和流自己已经在精确计数（`AudioConsumer::dropped_samples`、
    /// 回调里的本地计数），这里再累加一次只会把一个已经准确的数字
    /// 变成两倍的。绝对写入还有个好处：快照可以随时重取，
    /// 不会因为漏读一次就永久偏移。
    pub fn set_frames_in(&self, n: u64) {
        self.frames_in.store(n, Ordering::Relaxed);
    }

    pub fn frames_in(&self) -> u64 {
        self.frames_in.load(Ordering::Relaxed)
    }

    pub fn set_frames_out(&self, n: u64) {
        self.frames_out.store(n, Ordering::Relaxed);
    }

    pub fn frames_out(&self) -> u64 {
        self.frames_out.load(Ordering::Relaxed)
    }

    pub fn set_dropped_in(&self, n: u64) {
        self.dropped_in.store(n, Ordering::Relaxed);
    }

    pub fn dropped_in(&self) -> u64 {
        self.dropped_in.load(Ordering::Relaxed)
    }

    pub fn set_dropped_out(&self, n: u64) {
        self.dropped_out.store(n, Ordering::Relaxed);
    }

    pub fn dropped_out(&self) -> u64 {
        self.dropped_out.load(Ordering::Relaxed)
    }

    /// 分析线程 → 写入线程之间丢掉的块数。**增量**，因为它没有别的
    /// 权威来源（`try_send` 失败时只有调用方知道）。
    pub fn count_dropped_block(&self) {
        self.dropped_blocks.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dropped_blocks(&self) -> u64 {
        self.dropped_blocks.load(Ordering::Relaxed)
    }

    /// 从检测器的计数器整体同步一次。
    ///
    /// 参数是 [`noise_detect::DetectorCounters`] 的四个值而不是那个
    /// 结构体本身：`noise-audio` 不应该把「状态要怎么显示」这件事
    /// 变成对检测器内部结构的耦合，将来检测器加计数器不该逼着这里改。
    pub fn sync_detector_counters(
        &self,
        events_started: u64,
        impacts: u64,
        candidates_rejected: u64,
        suppressed_by_gate: u64,
    ) {
        self.events_started.store(events_started, Ordering::Relaxed);
        self.impacts.store(impacts, Ordering::Relaxed);
        self.candidates_rejected.store(candidates_rejected, Ordering::Relaxed);
        self.suppressed_by_gate
            .store(suppressed_by_gate, Ordering::Relaxed);
    }

    pub fn events_started(&self) -> u64 {
        self.events_started.load(Ordering::Relaxed)
    }

    pub fn impacts(&self) -> u64 {
        self.impacts.load(Ordering::Relaxed)
    }

    pub fn candidates_rejected(&self) -> u64 {
        self.candidates_rejected.load(Ordering::Relaxed)
    }

    pub fn suppressed_by_gate(&self) -> u64 {
        self.suppressed_by_gate.load(Ordering::Relaxed)
    }

    pub fn set_rt_violations(&self, n: u64) {
        self.rt_violations.store(n, Ordering::Relaxed);
    }

    pub fn rt_violations(&self) -> u64 {
        self.rt_violations.load(Ordering::Relaxed)
    }

    pub fn add_degraded(&self, flags: u64) {
        self.degraded.fetch_or(flags, Ordering::Relaxed);
    }

    pub fn clear_degraded(&self, flags: u64) {
        self.degraded.fetch_and(!flags, Ordering::Relaxed);
    }

    pub fn degraded(&self) -> u64 {
        self.degraded.load(Ordering::Relaxed)
    }

    // ---- 回放 ----

    pub fn set_playback(&self, active: bool) {
        self.playback_active.store(active, Ordering::Relaxed);
    }

    pub fn playback_active(&self) -> bool {
        self.playback_active.load(Ordering::Relaxed)
    }

    pub fn set_playback_gain_db(&self, db: f64) {
        self.playback_gain_db.store(pack_finite(db), Ordering::Relaxed);
    }

    /// 当前回放增益，dB。负值表示衰减。
    pub fn playback_gain_db(&self) -> f64 {
        unpack(self.playback_gain_db.load(Ordering::Relaxed)) as f64
    }

    // ---- 电平 ----

    pub fn set_levels(&self, t_ms: f64, dbfs: f64, floor_dbfs: f64) {
        // 会话时间戳截到 u32：`u32::MAX` 毫秒是 49.7 天，
        // 比任何一个监测会话都长，而它换来的是和无锁快照一样的
        // 单字对齐读取。
        self.t_ms.store(t_ms.max(0.0).min(u32::MAX as f64) as u32, Ordering::Relaxed);
        self.level_dbfs.store(pack_finite(dbfs), Ordering::Relaxed);
        self.floor_dbfs.store(pack_finite(floor_dbfs), Ordering::Relaxed);
    }

    pub fn level_dbfs(&self) -> f64 {
        unpack(self.level_dbfs.load(Ordering::Relaxed)) as f64
    }

    pub fn floor_dbfs(&self) -> f64 {
        unpack(self.floor_dbfs.load(Ordering::Relaxed)) as f64
    }

    pub fn t_ms(&self) -> f64 {
        self.t_ms.load(Ordering::Relaxed) as f64
    }

    /// 全部标量的一次性快照。给命令层用。
    pub fn snapshot(&self) -> StatusSnapshot {
        StatusSnapshot {
            run: self.run(),
            detector: self.detector(),
            gate: self.gate(),
            input: self.input(),
            output: self.output(),
            frames_in: self.frames_in(),
            frames_out: self.frames_out(),
            dropped_in: self.dropped_in(),
            dropped_out: self.dropped_out(),
            dropped_blocks: self.dropped_blocks(),
            events_started: self.events_started(),
            impacts: self.impacts(),
            candidates_rejected: self.candidates_rejected(),
            suppressed_by_gate: self.suppressed_by_gate(),
            rt_violations: self.rt_violations(),
            degraded: self.degraded(),
            degraded_notes: describe_degraded(self.degraded())
                .into_iter()
                .map(str::to_string)
                .collect(),
            playback_active: self.playback_active(),
            playback_gain_db: self.playback_gain_db(),
            level_dbfs: self.level_dbfs(),
            floor_dbfs: self.floor_dbfs(),
            t_ms: self.t_ms(),
        }
    }
}

fn detector_to_u8(s: DetectorState) -> u8 {
    match s {
        DetectorState::Idle => 0,
        DetectorState::Active => 1,
        DetectorState::Suspended => 2,
    }
}

fn detector_from_u8(v: u8) -> DetectorState {
    match v {
        1 => DetectorState::Active,
        2 => DetectorState::Suspended,
        _ => DetectorState::Idle,
    }
}

fn gate_to_u8(s: GateState) -> u8 {
    match s {
        GateState::Open => 0,
        GateState::Muted => 1,
        GateState::Cooldown => 2,
    }
}

fn gate_from_u8(v: u8) -> GateState {
    match v {
        1 => GateState::Muted,
        2 => GateState::Cooldown,
        _ => GateState::Open,
    }
}

/// [`MonitorStatus`] 的可序列化快照。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub run: RunState,
    pub detector: DetectorState,
    pub gate: GateState,
    /// 输入流**实际达成**的参数。`None` 表示没在跑。
    pub input: Option<StreamInfo>,
    pub output: Option<StreamInfo>,
    pub frames_in: u64,
    pub frames_out: u64,
    pub dropped_in: u64,
    pub dropped_out: u64,
    pub dropped_blocks: u64,
    pub events_started: u64,
    pub impacts: u64,
    pub candidates_rejected: u64,
    pub suppressed_by_gate: u64,
    pub rt_violations: u64,
    /// 降级位图，见 [`degraded`]。
    pub degraded: u64,
    /// 位图对应的人话，省得前端再维护一份映射表——两份映射表
    /// 迟早会分叉，而分叉的表现是 UI 上少显示一条警告。
    pub degraded_notes: Vec<String>,
    pub playback_active: bool,
    pub playback_gain_db: f64,
    pub level_dbfs: f64,
    pub floor_dbfs: f64,
    pub t_ms: f64,
}

/// `monitor://level` 事件的载荷。
///
/// **数值全部用有限值**。`serde_json` 把 `NaN` 和 `±Infinity` 序列化成
/// `null`，于是前端拿到的是一个缺失的字段而不是一个坏数字——前端代码
/// 里 `payload.dbfs.toFixed(1)` 会抛 `TypeError` 而不是显示一个怪值，
/// 而这两者的排查难度差得很远。所以写入侧统一用 `DB_FLOOR`（−200 dB）
/// 表示「没有信号」，而不是 `-Infinity`。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LevelSnapshot {
    /// 本次监测会话开始以来的毫秒数。
    pub t_ms: f64,
    /// 宽带 Z 计权电平，dBFS。**永远有值。**
    pub dbfs: f64,
    /// Z 计权声压级，dB SPL。**未校准时是 `None`。**
    ///
    /// 用 `Option` 而不是「未校准时填 dBFS」：后者会在报告里变成
    /// 「邻居达到 68 dB」，而那是一个站不住脚的主张。缺失就缺失。
    pub db_spl_z: Option<f64>,
    /// A 计权、Fast 时间计权，dBFS。
    pub db_a_fast: f64,
    /// A 计权、Slow 时间计权，dBFS。
    pub db_a_slow: f64,
    /// 本次事件以来的 A 计权等效声级。事件外是 `None`。
    pub leq_a: Option<f64>,
    /// 六个八度带（31.5 / 63 / 125 / 250 / 500 / 1000 Hz）的电平。
    pub band_db: [f64; 6],
    /// 触发链的底噪，dB。
    pub floor_db: f64,
    /// 当前起始门限，dB（绝对，已含底噪）。
    pub start_db: f64,
    /// 帧内峰均比，dB。连续噪声 3–6，冲击 10–20。
    pub crest_db: f64,
    /// 触发带（20–200 Hz，未计权）电平，dB。**这就是判据看的那个数。**
    pub trigger_db: f64,
    /// 八度带谱平坦度，dB。只做软标记，不参与判定。
    pub flatness: f64,
    pub state: DetectorState,
    pub gate: GateState,
    /// 是否已校准。UI 靠它决定显示「dBFS」还是「dB(A)」。
    pub calibrated: bool,
}

impl Default for LevelSnapshot {
    fn default() -> Self {
        // 手写而不是 derive：`DetectorState` 没有 `Default`，而且
        // 更重要的是**默认值必须是可序列化的有限数**——见类型文档。
        // 用 `-Infinity` 当「还没收到帧」的哨兵会让这个快照在
        // JSON 里凭空多出一堆 null。
        let f = noise_detect::DB_FLOOR;
        Self {
            t_ms: 0.0,
            dbfs: f,
            db_spl_z: None,
            db_a_fast: f,
            db_a_slow: f,
            leq_a: None,
            band_db: [f; 6],
            floor_db: f,
            start_db: f,
            crest_db: 0.0,
            trigger_db: f,
            flatness: 0.0,
            state: DetectorState::Idle,
            gate: GateState::Open,
            calibrated: false,
        }
    }
}

/// 带毒化的互斥快照。
///
/// # 为什么吃了毒化而不用 `unwrap()`
///
/// 毒化表示「持锁的线程 panic 了」。在这里那是**分析线程崩溃**，
/// 而它崩溃之后 UI 最不该做的事情是跟着一起崩——用户已经在一个
/// 出问题的监测器前面了，此时能显示「上一帧的电平」比白屏有用。
/// 而且这个锁后面的数据是一个纯值类型，没有任何「只写了一半」的
/// 不变量会被毒化破坏。
#[derive(Debug)]
pub struct SharedLevel(Mutex<LevelSnapshot>);

impl Default for SharedLevel {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedLevel {
    pub fn new() -> Self {
        Self(Mutex::new(LevelSnapshot::default()))
    }

    /// 写入。分析线程调用，20 Hz 左右。
    pub fn store(&self, snap: LevelSnapshot) {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *guard = snap;
    }

    /// 读一份拷贝。发射器调用。
    pub fn load(&self) -> LevelSnapshot {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// 共享句柄。命令层和发射器各持一份。
pub type SharedStatus = Arc<MonitorStatus>;

#[cfg(test)]
mod tests {
    use super::*;

    fn info(rate: u32) -> StreamInfo {
        StreamInfo {
            sample_rate: rate,
            channels: 2,
            format: "f32".into(),
            format_code: 10,
            bits: 32,
            buffer_frames: None,
        }
    }

    #[test]
    fn run_state_round_trips_through_the_atomic() {
        let s = MonitorStatus::new();
        assert_eq!(s.run(), RunState::Stopped);
        for r in [
            RunState::Starting,
            RunState::Running,
            RunState::Degraded,
            RunState::Stopping,
            RunState::Stopped,
        ] {
            s.set_run(r);
            assert_eq!(s.run(), r);
        }
    }

    /// 「只有第一个读者看到变化」是发射器的去重依据。返回 `Some`
    /// 两次会让状态栏事件推两遍，返回 `None` 一次会让 UI 永远卡在
    /// 旧状态上。
    #[test]
    fn run_change_is_edge_triggered_exactly_once() {
        let s = MonitorStatus::new();
        assert_eq!(s.take_run_change(), None, "初始状态不算一次变化");

        s.set_run(RunState::Running);
        assert_eq!(s.take_run_change(), Some(RunState::Running));
        assert_eq!(s.take_run_change(), None, "同一次变化不能被读走两次");

        s.set_run(RunState::Stopped);
        assert_eq!(s.take_run_change(), Some(RunState::Stopped));
        assert_eq!(s.take_run_change(), None);
    }

    /// 「打开了」是布尔量而不是 `sample_rate != 0`：一个真的报了 0 的
    /// 设备（驱动 bug）会被后者当成「没打开」，而那个 bug 恰恰是
    /// 需要被显示出来的那一种。
    #[test]
    fn open_flag_is_separate_from_the_sample_rate() {
        let s = MonitorStatus::new();
        assert_eq!(s.input(), None);

        let mut weird = info(0);
        weird.channels = 0;
        s.set_input(&weird);
        let got = s.input().expect("打开了就该有值");
        assert_eq!(got.sample_rate, 0);
        assert_eq!(got.channels, 0);

        s.clear_input();
        assert_eq!(s.input(), None);
    }

    #[test]
    fn input_and_output_do_not_share_slots() {
        let s = MonitorStatus::new();
        s.set_input(&info(48_000));
        s.set_output(&info(96_000));
        assert_eq!(s.input().unwrap().sample_rate, 48_000);
        assert_eq!(s.output().unwrap().sample_rate, 96_000);

        s.clear_input();
        assert_eq!(s.input(), None);
        assert_eq!(
            s.output().unwrap().sample_rate,
            96_000,
            "清掉输入不该影响输出"
        );
    }

    /// **非有限值绝不能进入快照。**
    ///
    /// `serde_json` 把 `NaN` / `±Infinity` 序列化成 `null`，前端拿到的是
    /// 一个缺失的字段而不是一个坏数字——`payload.dbfs.toFixed(1)` 会抛
    /// `TypeError`，而排查一个「UI 白屏」比排查一个显示 `-200.0` 难得多。
    ///
    /// 判据是「**数字字段序列化出来还是数字**」，不是「JSON 里没有 null」：
    /// 快照里的 `Option` 字段（未校准时的 dB SPL）本来就该是 null，
    /// 那是**有意的**缺失。把两者混为一谈的断言会一直误报，然后被删掉。
    #[test]
    fn non_finite_levels_are_replaced_not_forwarded() {
        let s = MonitorStatus::new();
        s.set_levels(f64::NAN, f64::NEG_INFINITY, f64::INFINITY);
        assert_eq!(s.level_dbfs(), noise_detect::DB_FLOOR as f64);
        assert_eq!(s.floor_dbfs(), noise_detect::DB_FLOOR as f64);
        assert_eq!(s.t_ms(), 0.0, "NaN 时间戳应被钳到 0");

        s.set_playback_gain_db(f64::NAN);
        assert!(s.playback_gain_db().is_finite());

        let v = serde_json::to_value(s.snapshot()).expect("快照必须能序列化");
        for key in ["level_dbfs", "floor_dbfs", "playback_gain_db", "t_ms"] {
            assert!(
                v[key].is_number(),
                "{key} 应该是数字，实得 {}（非有限值被 serde 吃成 null 了）",
                v[key]
            );
        }
        // 未建流时 input/output 是 null，这是**对的**——没在跑就是没在跑。
        assert!(v["input"].is_null());
    }

    /// 逐字段检查默认快照的序列化结果：每个数字字段都必须是数字，
    /// 每个有意缺失的字段都必须是 null，其余各按类型。
    ///
    /// 遍历全部键而不是抽查几个，是因为**将来新增字段时这条测试会自动
    /// 覆盖到它**——落进 `_` 分支的字段如果是个字符串，测试会立刻失败，
    /// 而那正是应该有人去更新这份清单的时刻。
    #[test]
    fn default_level_snapshot_serializes_field_by_field() {
        let v = serde_json::to_value(LevelSnapshot::default()).unwrap();
        let obj = v.as_object().expect("应该是个对象");

        for (k, val) in obj {
            match k.as_str() {
                // 有意的缺失：没校准就不主张绝对声压级，没有事件就没有 Leq。
                "db_spl_z" | "leq_a" => {
                    assert!(val.is_null(), "{k} 未校准/无事件时应为 null，实得 {val}");
                }
                "band_db" => {
                    let arr = val.as_array().expect("band_db 应是数组");
                    assert_eq!(arr.len(), 6, "六个八度带");
                    assert!(arr.iter().all(serde_json::Value::is_number), "{val}");
                }
                "state" | "gate" => assert!(val.is_string(), "{k} 实得 {val}"),
                "calibrated" => assert!(val.is_boolean(), "{k} 实得 {val}"),
                _ => assert!(
                    val.is_number(),
                    "{k} 是数字字段，实得 {val}；新加的字段请在上面的清单里登记"
                ),
            }
        }

        // 字段本身要在，别哪天顺手删了却没人发现
        for k in ["db_spl_z", "leq_a", "band_db", "calibrated", "trigger_db"] {
            assert!(obj.contains_key(k), "快照少了字段 {k}");
        }
    }

    /// 校准之后 dB SPL 字段必须真的填上——否则 UI 会永远显示
    /// 「未校准」，用户以为校准没生效。
    #[test]
    fn calibration_fills_in_the_spl_fields() {
        let snap = LevelSnapshot {
            calibrated: true,
            db_spl_z: Some(64.5),
            leq_a: Some(58.0),
            ..LevelSnapshot::default()
        };
        let v = serde_json::to_value(&snap).unwrap();
        assert_eq!(v["db_spl_z"], serde_json::json!(64.5));
        assert_eq!(v["leq_a"], serde_json::json!(58.0));
        assert_eq!(v["calibrated"], serde_json::json!(true));
    }

    /// 计数器是绝对写入，重复同步同一个值不该翻倍。
    #[test]
    fn counters_are_absolute_not_accumulated() {
        let s = MonitorStatus::new();
        s.sync_detector_counters(5, 9, 2, 1);
        s.sync_detector_counters(5, 9, 2, 1);
        assert_eq!(s.events_started(), 5);
        assert_eq!(s.impacts(), 9);
        assert_eq!(s.candidates_rejected(), 2);
        assert_eq!(s.suppressed_by_gate(), 1);
    }

    #[test]
    fn dropped_block_is_accumulated() {
        let s = MonitorStatus::new();
        s.count_dropped_block();
        s.count_dropped_block();
        assert_eq!(s.dropped_blocks(), 2);
    }

    /// 降级位图是**或**关系：两个问题同时存在时两个位都要在，
    /// 而且清掉一个不能顺手把另一个也清掉。
    #[test]
    fn degraded_flags_are_independent_bits() {
        let s = MonitorStatus::new();
        s.add_degraded(degraded::DROPPED_AUDIO);
        s.add_degraded(degraded::UNCALIBRATED);
        assert_eq!(s.degraded(), degraded::DROPPED_AUDIO | degraded::UNCALIBRATED);

        s.clear_degraded(degraded::DROPPED_AUDIO);
        assert_eq!(s.degraded(), degraded::UNCALIBRATED);
        assert_eq!(describe_degraded(s.degraded()).len(), 1);
    }

    /// 每一位都要有对应的人话，否则 UI 上会出现一条「降级了但没说为什么」。
    #[test]
    fn every_degraded_bit_has_a_description() {
        let all = [
            degraded::SAMPLE_RATE_CLAMPED,
            degraded::DSP_FILTER_CLAMPED,
            degraded::DROPPED_AUDIO,
            degraded::RT_VIOLATION,
            degraded::RUNAWAY_SUSPENDED,
            degraded::DEVICE_RESTARTED,
            degraded::EVIDENCE_INCOMPLETE,
            degraded::UNCALIBRATED,
        ];
        for (i, bit) in all.iter().enumerate() {
            let notes = describe_degraded(*bit);
            assert_eq!(notes.len(), 1, "第 {i} 位没有（或有重复的）说明");
            assert!(!notes[0].is_empty());
        }
        // 全开时一位不落
        assert_eq!(describe_degraded(all.iter().fold(0, |a, b| a | b)).len(), all.len());
        assert!(describe_degraded(0).is_empty());
    }

    /// 毒化不能永久毁掉 UI。分析线程 panic 之后，用户至少还能看到
    /// 最后那一帧电平——那正是排查所需的线索。
    #[test]
    fn a_poisoned_level_mutex_still_reads() {
        let shared = Arc::new(SharedLevel::new());
        let victim = Arc::clone(&shared);
        let _ = std::thread::spawn(move || {
            let _guard = victim.0.lock().unwrap();
            panic!("持锁时崩溃");
        })
        .join();

        let mut snap = LevelSnapshot::default();
        snap.dbfs = -12.0;
        shared.store(snap.clone());
        assert_eq!(shared.load().dbfs, -12.0, "毒化之后仍然读写得了");
    }
}
