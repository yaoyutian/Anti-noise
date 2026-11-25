//! 输入流：设备格式 → 单声道 f32 → 无锁环。
//!
//! # 这一层的全部职责
//!
//! 在实时回调的约束下，把设备给的任何东西变成一条**原始、连续、可计账**
//! 的单声道流。
//!
//! * **原始**——这里不做任何滤波。触发链的 20–200 Hz 带通在分析线程里，
//!   录音写的是这里的输出。把滤波后的信号写进磁盘就会毁掉证据，
//!   理由见 crate 文档。
//! * **连续**——丢样本可以（环满了），但**悄悄丢**不可以。丢掉的个数
//!   在 [`AudioProducer::dropped_samples`] 里，一路报到 UI。
//! * **可计账**——引擎要能算出「设备一共给了多少个样本」，而这个数
//!   必须由回调之外的地方算得出来，因为回调里不能写日志。
//!
//! # 为什么要把块再切成小段
//!
//! `Downmixer` 的暂存区是预先分配的（分配在回调里是禁止的），所以它
//! 有固定容量。真实设备的块大小通常远小于它，但**这不是保证**——
//! 某些后端在设备刚打开的第一块、或者驱动状态异常时会给出异常大的
//! 缓冲。不切块的话，那时要不在回调里扩容（爆音），要不静默丢弃
//! 整个块（更糟：一段凭空消失的录音）。
//!
//! 切块时**按帧对齐**：切点落在帧边界上，不然立体声设备的每一段都会
//! 丢掉最后一个残缺帧，而那个丢失是累积的——一秒钟下来就是几百个
//! 样本的系统性缺口。
//!
//! # 通道数
//!
//! 单声道设备走的就是 `ch == 1` 那条路，没有额外分支。多声道的降混
//! 约定（算术平均，不是 `1/√N`）在 `convert.rs` 里，包括为什么。

use cpal::traits::DeviceTrait;
use cpal::SampleFormat;

use crate::convert::{Downmixer, DEFAULT_CAPACITY_FRAMES};
use crate::device::{
    describe_cpal_error, device_name, is_capturable, negotiate, ErrorCounters, ErrorSink,
    Negotiated, StreamInfo,
};
use crate::ring::{AudioConsumer, AudioProducer};
use crate::rtguard::RtGuard;

/// 建输入流的结果。
///
/// `stream` **必须由建它的那个线程持有**。cpal 的 `Stream` 不保证
/// `Send`（0.18 的 `platform_impl` 宏没有为它发射 `unsafe impl Send`，
/// coreaudio 后端也没有），所以把它移进另一个线程会编译失败，或者
/// 在某些后端上运行时出问题。整个引擎因此是「引擎线程自己建流、
/// 自己持有」的结构，见 `engine.rs`。
pub struct InputBuild {
    pub stream: cpal::Stream,
    /// 分析线程从这里取样本。
    pub consumer: AudioConsumer,
    /// 实际达成的参数。**这个要显示到 UI 上**，不是请求的那份。
    pub info: StreamInfo,
    /// 协商过程中被降级的地方。
    pub notes: Vec<String>,
    pub errors: crossbeam_channel::Receiver<cpal::Error>,
    pub error_counters: std::sync::Arc<ErrorCounters>,
}

impl InputBuild {
    /// 启动输入流。
    ///
    /// cpal 的流建好后是否自动开始依后端而定，显式调用一次最稳。
    /// 错误转成字符串，避免把 cpal 的错误类型再泄漏给调用方。
    pub fn play(&mut self) -> Result<(), String> {
        use cpal::traits::StreamTrait;
        self.stream
            .play()
            .map_err(|e| format!("启动输入流失败：{e}"))
    }
}

/// 建输入流。
///
/// `ring_capacity_samples` 是环的容量；2 秒（`sample_rate * 2`）是引擎
/// 用的值。环越短，分析线程一次卡顿就丢得越多；越长，事件触发时那段
/// 「已经过去但还没被分析」的音频就越多，pre-roll 的索引也就越难对齐。
///
/// 降混暂存区**不可配置**，固定用 [`DEFAULT_CAPACITY_FRAMES`]：它是
/// 64 KB 的预分配缓冲，比任何真实设备给出的块大一个数量级，而把它做成
/// 一个参数只会多出一个没人会去调的旋钮——调小它会让异常大的设备块被
/// 切成更多片（多一些开销），调大它没有任何收益。
pub fn build_input_stream(
    device: &cpal::Device,
    sample_rate: Option<u32>,
    ring_capacity_samples: usize,
) -> Result<InputBuild, noise_core::CoreError> {
    let neg: Negotiated = negotiate(device, true, sample_rate, None)?;

    let (producer, consumer) = AudioProducer::new(ring_capacity_samples);
    let (sink, errors, error_counters) = ErrorSink::new();

    let downmix = Downmixer::new(neg.config.channels as usize, DEFAULT_CAPACITY_FRAMES);

    let stream = dispatch_input(device, &neg, producer, downmix, sink)?;

    Ok(InputBuild {
        stream,
        consumer,
        info: StreamInfo::new(&neg.config, neg.format),
        notes: neg.notes,
        errors,
        error_counters,
    })
}

/// 12 种数值格式的分派。理由同 `output.rs`：闭包只能按值移动一次。
macro_rules! build_typed {
    ($device:expr, $config:expr, $ch:expr, $producer:expr, $downmix:expr, $sink:expr, $t:ty) => {{
        let mut producer = $producer;
        let mut downmix = $downmix;
        let sink = $sink;
        let ch = $ch;
        $device.build_input_stream::<$t, _, _>(
            $config,
            move |data: &[$t], _: &cpal::InputCallbackInfo| {
                // 回调边界。debug + rt-guard 构建下，这里之后再分配
                // 就会被记账（见 `rtguard`）。
                let _rt = RtGuard::enter();

                // 按帧对齐地切块。见模块文档。
                let cap = downmix.capacity_samples();
                let mut off = 0usize;
                while off < data.len() {
                    let mut end = (off + cap).min(data.len());
                    // 让 `end - off` 是 `ch` 的整数倍。`cap` 本身就是
                    // `帧数 * ch`，所以只有最后一块需要这一步。
                    let span = end - off;
                    end -= span % ch;
                    if end == off {
                        // 剩下的不足一帧。丢掉——半帧凑不出一个样本，
                        // 而把它当成一帧会在后面所有样本上引入错位。
                        break;
                    }
                    let mono = downmix.downmix_into(&data[off..end]);
                    producer.push_partial(mono);
                    off = end;
                }
            },
            move |err| sink.report(err),
            None,
        )
    }};
}

fn dispatch_input(
    device: &cpal::Device,
    neg: &Negotiated,
    producer: AudioProducer,
    downmix: Downmixer,
    sink: ErrorSink,
) -> Result<cpal::Stream, noise_core::CoreError> {
    let config = neg.config.clone();
    let channels = config.channels as usize;
    let name = device_name(device);

    let stream = match neg.format {
        SampleFormat::I8 => build_typed!(device, config, channels, producer, downmix, sink, i8),
        SampleFormat::I16 => build_typed!(device, config, channels, producer, downmix, sink, i16),
        SampleFormat::I24 => {
            build_typed!(device, config, channels, producer, downmix, sink, cpal::I24)
        }
        SampleFormat::I32 => build_typed!(device, config, channels, producer, downmix, sink, i32),
        SampleFormat::I64 => build_typed!(device, config, channels, producer, downmix, sink, i64),
        SampleFormat::U8 => build_typed!(device, config, channels, producer, downmix, sink, u8),
        SampleFormat::U16 => build_typed!(device, config, channels, producer, downmix, sink, u16),
        SampleFormat::U24 => {
            build_typed!(device, config, channels, producer, downmix, sink, cpal::U24)
        }
        SampleFormat::U32 => build_typed!(device, config, channels, producer, downmix, sink, u32),
        SampleFormat::U64 => build_typed!(device, config, channels, producer, downmix, sink, u64),
        SampleFormat::F32 => build_typed!(device, config, channels, producer, downmix, sink, f32),
        SampleFormat::F64 => build_typed!(device, config, channels, producer, downmix, sink, f64),
        other => {
            // 正常走不到：`negotiate` 用 `is_capturable` 把 DSD 挡在外面，
            // 因为 dasp 没有为它们实现 `Sample`，泛型实例化不出来。
            return Err(noise_core::CoreError::DeviceUnavailable {
                device: name,
                reason: format!("不支持的采样格式 {other:?}"),
            });
        }
    }
    .map_err(|e: cpal::Error| noise_core::CoreError::DeviceUnavailable {
        device: name.clone(),
        reason: describe_cpal_error(&e),
    })?;

    Ok(stream)
}

/// 已经产出但**还没被分析线程取走**的样本数：躺在环里的 + 被丢掉的。
///
/// # 为什么这个数由消费侧算而不是由回调记
///
/// 回调里每一次原子 `fetch_add` 都是实时预算里的一笔开销，而这个数
/// **恰好可以从消费侧已有的量推出来**：
///
/// ```text
/// 设备产出总数 = 分析线程已取走的 + pending_samples()
/// ```
///
/// 右两项都在消费侧可读，所以回调里一个额外原子量都不用加。这类
/// 「本来可以算出来的数就不要单独维护」的取舍在实时路径上值得反复用
/// ——每多一个共享可变状态，就多一处回调和分析线程之间的顺序假设。
///
/// **注意「已取走的」不在这个函数的返回值里**：只有分析线程自己知道
/// 它取走了多少，所以那一项由分析线程维护自己的计数（见 `analysis.rs`
/// 的 `popped`），两边相加才是设备产出总数。
///
/// 丢掉的样本**算在产出里**：设备确实吐出来了，只是我们没能接住。
/// 把它们排除掉的话，UI 上「一共收到多少」会随着丢包悄悄变小，而
/// 那正是最需要被看见的数字。
pub fn pending_samples(consumer: &AudioConsumer) -> u64 {
    consumer.dropped_samples() + consumer.len() as u64
}

/// 设备承诺支持的格式组合里，有多少种我们能真的打开。
///
/// 给 `probe_device` 命令用：让用户看到「15 种组合，其中 12 种可用」，
/// 比列出 15 种然后在建流时失败要好得多。
///
/// `FormatRange::format` 是**名字**（`format_name` 的产物，便于人读 JSON），
/// 不是 `SampleFormat`——所以这里要先转回来。转不回来的（将来 cpal 加了
/// 新变体而我们还没跟上）算作不可用，这是保守但正确的方向。
pub fn usable_format_count(formats: &[crate::device::FormatRange]) -> usize {
    formats
        .iter()
        .filter(|f| {
            crate::device::format_from_code(f.format_code).is_some_and(is_capturable)
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 分块逻辑的核心：不管设备给多大的块，进环的单声道流必须一模一样。
    ///
    /// 这条测试不碰 cpal——它把回调体里那段分块逻辑原样搬出来跑。
    /// 分块是纯粹的索引算术，而索引算术正是「设备换了块大小之后录音里
    /// 凭空少了一截」这类 bug 的产地。
    ///
    /// # 测试数据必须留在 ±1.0 以内
    ///
    /// 超出的部分会被 `Downmixer` 按约定**钳住**（`convert.rs` 的
    /// `sanitize`）。用 `0..N` 当测试数据的话，整条流除了开头全变成
    /// 1.0，看起来像是分块把数据搞坏了——而实际是钳位在正常工作。
    /// 下面的 `tone()` 保证每个值都在带内，并且是 2 的幂等分，
    /// 比较不用容差。
    /// `N` 个在 ±1.0 内、二进制精确可表示的样本。
    fn tone(n: usize) -> Vec<f32> {
        (0..n).map(|i| (i as f32 + 1.0) / 128.0).collect()
    }

    fn chunked_downmix(channels: usize, capacity_frames: usize, data: &[f32]) -> Vec<f32> {
        let mut downmix = Downmixer::new(channels, capacity_frames);
        let mut out = Vec::new();
        let ch = channels.max(1);
        let cap = downmix.capacity_samples();
        let mut off = 0usize;
        while off < data.len() {
            let mut end = (off + cap).min(data.len());
            let span = end - off;
            end -= span % ch;
            if end == off {
                break;
            }
            out.extend_from_slice(downmix.downmix_into(&data[off..end]));
            off = end;
        }
        out
    }

    /// 设备给的块比分块容量大：必须全部拿到，顺序不能乱。
    #[test]
    fn an_oversized_device_block_is_fully_consumed() {
        // 容量 5 帧，设备给 23 个样本（单声道）→ 5+5+5+5+3
        let data = tone(23);
        let got = chunked_downmix(1, 5, &data);
        assert_eq!(got, data, "比容量大的块必须被完整吃掉");
    }

    /// 立体声下切点必须落在帧边界上。
    ///
    /// 不按帧对齐的话，每次切块都会丢掉最后那一个残缺帧，而那个丢失
    /// 是**累积**的——48 kHz 下每秒几百个样本的系统性缺口。
    #[test]
    fn chunking_never_splits_a_frame() {
        // 容量 5 帧 = 10 个样本；设备给 2 声道 × 13 帧 = 26 个样本
        let mono = tone(13);
        let mut data = Vec::new();
        for &v in &mono {
            data.push(v); // 左
            data.push(v); // 右（相同 → 降混后不变）
        }
        let got = chunked_downmix(2, 5, &data);
        assert_eq!(got.len(), 13, "13 帧一帧都不能少");
        assert_eq!(got, mono, "切点落在帧边界上，逐帧必须完全一致");
    }

    /// 不足一帧的残片只能丢掉，不能当成一帧。
    ///
    /// 把它当一帧的后果不是「少一个样本」，而是**后面所有帧都错位**：
    /// 右声道的数据会被当作左声道，整个录音听起来是错的但完全不像
    /// 数字故障。
    #[test]
    fn a_partial_frame_at_the_end_is_dropped() {
        // 2 声道，容量 4 帧，给 9 个样本 = 4 帧 + 1 个残片
        let data: Vec<f32> = vec![0.5; 9];
        let got = chunked_downmix(2, 4, &data);
        assert_eq!(got.len(), 4);
    }

    /// 设备给空块（某些后端在流刚开始时会这样）不能死循环、不能 panic。
    #[test]
    fn an_empty_block_is_harmless() {
        assert!(chunked_downmix(2, 8, &[]).is_empty());
    }

    /// 容量为 1 帧时每一块只处理一帧——慢，但必须正确。
    #[test]
    fn a_one_frame_capacity_still_produces_the_whole_stream() {
        assert_eq!(chunked_downmix(1, 1, &tone(40)), tone(40));
    }

    /// 产出总数 = 分析线程已取走 + `pending_samples()`。
    ///
    /// 这个等式是 UI 上「一共收到多少」和 `dropped_in` 两个数的唯一依据，
    /// 算错了用户就会看到「丢了 0 个但总接收数对不上」。
    #[test]
    fn pending_samples_completes_the_production_ledger() {
        let (mut p, mut c) = AudioProducer::new(8);
        // 分析线程自己维护的那一项。见函数文档。
        let mut popped = 0u64;
        // 设备产出总数 = 已取走 + pending_samples()
        let produced = |popped: u64, c: &AudioConsumer| popped + pending_samples(c);

        assert_eq!(produced(popped, &c), 0);

        p.push_partial(&[0.0; 5]);
        assert_eq!(produced(popped, &c), 5);

        let mut out = [0.0f32; 3];
        assert_eq!(c.pop_into(&mut out), 3);
        popped += 3;
        assert_eq!(produced(popped, &c), 5, "取走的那 3 个仍然计入产出");
        assert_eq!(pending_samples(&c), 2, "还没被取走的只剩 2 个");

        // 再推 10 个进 8 格的环（已用 2，空 6）：装得下 6 个，丢 4 个。
        assert_eq!(p.push_partial(&[0.0; 10]), 4);
        assert_eq!(pending_samples(&c), 12, "环满 8 + 丢弃 4");
        assert_eq!(produced(popped, &c), 15, "3 取走 + 8 在环 + 4 丢弃 = 15 个产出");
    }
}
