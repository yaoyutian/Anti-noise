//! 回放回调：增益与生命周期纪律。
//!
//! # 这个模块唯一真正的难题
//!
//! 反击声的响度是**用户实测调出来的**，而调音量的动作会随时发生——包括
//! 在波形正在播放的时候。增益一旦跳变，`x[i-1] * g1` 和 `x[i] * g2` 之间
//! 就出现一个台阶，听感上是一声「咔」。所以增益必须**斜坡**。
//!
//! 但斜坡不能被无条件使用，否则会毁掉整个功能：
//!
//! * **起播时不能淡入。** 波形自己的起音是 8 ms 的升余弦（见
//!   `response.rs`），那是反击声里唯一有冲击力的部分。再叠一层 8 ms 的
//!   增益淡入，起音就变成了 16 ms 的软包络，敲在楼板上听起来像有人在
//!   隔壁关门。
//! * **停止时必须淡出。** 急停、看门狗、或者用户按下停止，都是在一个
//!   任意相位上打断波形。若此刻直接归零，最后一个样本从（比如）0.6 跳
//!   到 0，那是一个满幅阶跃——比不淡出更响的一声咔。
//!
//! 判据因此是**边沿**而不是电平：`idle → playing` 的那次跃迁直接跳到
//! 目标增益，其余任何增益变化都走斜坡。
//!
//! 波形本身以**恰好 0** 开头和结尾（`response.rs` 的 fade-out 分母是
//! `fade - 1`，见那里的注释），所以起播和自然播完这两处根本不需要淡。
//! 这条性质是跨模块的契约，`the_waveform_contract_holds` 钉住它。
//!
//! # 「静音先于第一个样本」
//!
//! 半双工互锁的顺序是：**置 `playing` → 压入波形**。反过来的话，输出
//! 回调会在波形入库之前就把 `playing` 读到 true 并渲染一个空块——代价
//! 只是一块静音，但更重要的是下面这件事：
//!
//! 空闲时回调**会清空环**。所以残留的波形不会跨越一次回放留到下一次。
//! 而与「清空」并存的唯一安全顺序就是先置 `playing`：先压入再置位的话，
//! 回调会在两次调用之间把刚压进去的波形当残留清掉，表现为**回放偶尔
//! 完全没声音，偶尔只剩后半段**——一个和时序赛跑、极难复现的 bug。
//!
//! # 错误回调也不许分配
//!
//! 它和数据回调一样可能跑在实时线程上，所以走的是同一套纪律：
//! [`ErrorSink`] 用有界通道 + `try_send`，既不阻塞也不分配。错误回调
//! **不**进入 `RtGuard`，因为 `RtGuard` 是给数据路径用的闸门，而库自己
//! 的报错路径不该污染「实时纪律是否被遵守」这个信号。
//!
//! 代价是有界通道满了就丢，所以丢掉的条数单独计数并报给 UI——
//! 「设备一直在报错但一条都没送到」和「设备一直正常」在界面上长得
//! 一模一样，而这两件事的排查方向差着十万八千里。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use cpal::traits::DeviceTrait;
use cpal::{FromSample, SampleFormat};

use crate::convert::DEFAULT_CAPACITY_FRAMES;
use crate::device::{
    describe_cpal_error, device_name, negotiate, ErrorCounters, ErrorSink, Negotiated, StreamInfo,
};
use crate::ring::{AudioConsumer, AudioProducer};
use crate::rtguard::RtGuard;

/// 增益上限。
///
/// 波形已经归一化到 −1 dBFS（`response.rs` 的 `PEAK_DBFS`），所以 0 dB
/// 就是「已经用满动态余量」。再往上只会削波，而削波产生的谐波正好落在
/// 触发链**不看**的频段里——它会变响、变难听、并且让实测耦合度失真，
/// 但不会让功能更有效。用户看到的是「调到 +6 之后反而更容易失控」。
///
/// 所以这是一个物理上限，不受配置影响。引擎的 `max_gain_db` 是用户侧
/// 的上限，这个是底线，两者取小。
pub const MAX_GAIN_DB: f64 = 0.0;

/// 增益下限。低于 −80 dB 等同于静音，继续往下没有意义。
pub const MIN_GAIN_DB: f64 = -80.0;

/// 急停/打断的淡出时长，秒。
///
/// 8 ms 短到听不出「关小」的过程，长到在 55 Hz（一个周期 18 ms）上仍有
/// 半个周期的余量把台阶摊开。再短就退回成咔哒声了。
pub const FADE_OUT_S: f64 = 0.008;

/// 停播判定：增益降到这个值以下就当作已经静音。
const GAIN_EPSILON: f64 = 1e-4;

/// 连续多少块拿不到样本就判定引擎违约，走淡出收尾。
///
/// 100 块在 48 kHz / 480 帧下是 1 秒。这个数字要大到能容忍一次调度
/// 抖动（分析线程被抢占了 200 ms 是完全可能的），小到不会让门控被
/// 永久锁住——`playing` 一直为真，半双工就不会解除静音，
/// 而**用户会看到麦克风明明正常但软件一直是聋的**。
const MAX_STARVED_BLOCKS: u32 = 100;

/// dB → 线性增益。非有限值当作静音。
pub fn db_to_linear(db: f64) -> f64 {
    if db.is_finite() {
        10f64.powf(db / 20.0)
    } else if db > 0.0 {
        // +inf：钳到上限而不是返回 inf，inf 乘出来的东西会污染整条
        // 输出链，而输出链的尽头是用户的大音响。
        10f64.powf(MAX_GAIN_DB / 20.0)
    } else {
        0.0
    }
}

/// 线性增益 → dB。0 映射到 [`noise_detect::DB_FLOOR`] 而不是 −∞：
/// −∞ 经 `serde_json` 会变成 `null`，前端 `.toFixed()` 就会抛。
pub fn linear_to_db(x: f64) -> f64 {
    if x > 0.0 && x.is_finite() {
        20.0 * x.log10()
    } else {
        noise_detect::DB_FLOOR
    }
}

/// 起播请求被接受之后实际生效的增益。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GainPlan {
    /// 调用方要求的。
    pub requested_db: f64,
    /// 实际用的。
    pub applied_db: f64,
    /// 被 [`MAX_GAIN_DB`] / [`MIN_GAIN_DB`] 钳过。**必须报给 UI**——
    /// 用户把滑块推到顶却听不出差别时，唯一的解释就在这里。
    pub clamped: bool,
}

/// 输出回调与引擎线程之间的控制面。全是原子量，回调只读。
///
/// 所有 `Ordering` 都是 `Relaxed`：这里的每一个字段都是**独立**的，
/// 它们之间没有需要跨线程维持的不变式。引擎不需要「看到 playing 就一定
/// 能看到刚写的增益」——恰恰相反，它需要的是「增益先落盘，playing 后
/// 置位」，而那由 `begin` 里的**写顺序**保证，不由内存序保证。
/// 用 `SeqCst` 只会买到一个并不存在的保证。
#[derive(Debug)]
pub struct PlaybackControl {
    /// 目标线性增益的 f32 位模式。
    gain_bits: AtomicU32,
    /// 引擎置位起播；回调在播完/打断后清零。
    playing: AtomicBool,
    /// 引擎请求淡出停止。
    stopping: AtomicBool,
    /// 回调侧已经完全静音、可以接受下一次起播。
    idle: AtomicBool,
    /// 本次回放是**自然播完**的（不是被打断的）。
    finished: AtomicBool,
    /// 回调渲染出去的样本数。
    rendered: AtomicU64,
    /// 波播到一半环空了——引擎喂得不够快。正常情况下恒为 0。
    underruns: AtomicU64,
    /// 本次回放输出过的最大绝对值。
    peak_bits: AtomicU32,
    /// 起播次数，用于对齐引擎侧的账。
    starts: AtomicU64,
    /// 本次回放**承诺**要渲染的样本数。见 [`PlaybackControl::begin`]。
    expected: AtomicU64,
}

impl Default for PlaybackControl {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackControl {
    pub fn new() -> Self {
        Self {
            gain_bits: AtomicU32::new(0.0f32.to_bits()),
            playing: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            idle: AtomicBool::new(true),
            finished: AtomicBool::new(false),
            rendered: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            peak_bits: AtomicU32::new(0.0f32.to_bits()),
            starts: AtomicU64::new(0),
            expected: AtomicU64::new(0),
        }
    }

    /// 起播。`expected_samples` 是引擎**承诺**会送进来的样本数。
    ///
    /// 返回 `None` 表示**拒绝**：上一次回放还没淡出完。这不是异常，
    /// 是半双工该有的行为——引擎把它记成一次 `suppressed` 而不是错误。
    ///
    /// # 为什么要让回调知道「一共多少个」
    ///
    /// 回调只看得到环，而环空了有两种截然不同的含义：波形放完了，
    /// 还是引擎少给了。这两者**在耳边的区别是一声咔**——波形若被截断
    /// 在中途，最后一个样本停在任意相位上，而用户的大音响会忠实地
    /// 把它变成一个阶跃。回调自己分不出来，所以引擎必须告诉它。
    ///
    /// 承诺值对不上时的处置见 [`PlaybackVoice::render_mono`]：不是直接
    /// 收工，而是走和急停一样的淡出路径。
    ///
    /// 顺序同样重要：**增益和承诺值先写，`playing` 后置**。回调在读到
    /// `playing` 之前不可能看到它们，所以起播那一块不会按旧参数渲一帧。
    pub fn begin(&self, gain_db: f64, expected_samples: u64) -> Option<GainPlan> {
        // 用 CAS 而不是「读 idle，再写 false」：两个 `begin` 并发时后者
        // 会覆盖前者，结果是两次回放叠在同一个环里。
        if self
            .idle
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }

        let requested = if gain_db.is_finite() { gain_db } else { MIN_GAIN_DB };
        let applied = requested.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
        let linear = db_to_linear(applied) as f32;

        self.gain_bits.store(linear.to_bits(), Ordering::Relaxed);
        self.expected.store(expected_samples, Ordering::Relaxed);
        self.stopping.store(false, Ordering::Relaxed);
        self.finished.store(false, Ordering::Relaxed);
        self.rendered.store(0, Ordering::Relaxed);
        self.underruns.store(0, Ordering::Relaxed);
        self.peak_bits.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.starts.fetch_add(1, Ordering::Relaxed);
        // 最后一步。之前写的所有东西对回调可见的**最晚**时刻就是这里。
        self.playing.store(true, Ordering::Release);

        Some(GainPlan {
            requested_db: requested,
            applied_db: applied,
            clamped: (applied - requested).abs() > f64::EPSILON,
        })
    }

    /// 请求淡出停止。回调会在几毫秒内把增益压到零，然后回到 idle。
    ///
    /// 幂等。对没在播的回调调用它没有任何副作用——回调根本不看这个位。
    pub fn request_stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }

    /// 当前目标线性增益。
    pub fn gain_linear(&self) -> f64 {
        f32::from_bits(self.gain_bits.load(Ordering::Relaxed)) as f64
    }

    pub fn gain_db(&self) -> f64 {
        linear_to_db(self.gain_linear())
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    /// 可以接受下一次起播。
    pub fn is_idle(&self) -> bool {
        self.idle.load(Ordering::Relaxed)
    }

    /// 读取并清除「上一次是自然播完的」。
    pub fn take_finished(&self) -> bool {
        self.finished.swap(false, Ordering::AcqRel)
    }

    pub fn rendered_samples(&self) -> u64 {
        self.rendered.load(Ordering::Relaxed)
    }

    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    pub fn starts(&self) -> u64 {
        self.starts.load(Ordering::Relaxed)
    }

    /// 本次回放承诺要渲染的样本数。
    pub fn expected_samples(&self) -> u64 {
        self.expected.load(Ordering::Relaxed)
    }

    /// 承诺的样本是不是都渲染完了。
    pub fn is_complete(&self) -> bool {
        match self.expected_samples() {
            0 => false,
            n => self.rendered.load(Ordering::Relaxed) >= n,
        }
    }

    /// 本次回放输出过的最大绝对值（线性）。
    pub fn peak_linear(&self) -> f64 {
        f32::from_bits(self.peak_bits.load(Ordering::Relaxed)) as f64
    }

    pub fn peak_dbfs(&self) -> f64 {
        linear_to_db(self.peak_linear())
    }

    /// 播放中（含淡出），或者还有波形没放完。
    pub fn is_active(&self) -> bool {
        !self.is_idle()
    }
}

/// 回放回调的全部状态。
///
/// **不含任何 cpal 类型**，所以整个状态机可以在没有声卡的机器上测。
/// 这是本模块最重要的结构决策：上面那些边沿条件（起播不淡入、停止必淡出、
/// 空闲清环）全都是时序 bug 的高发区，而它们在真实硬件上复现一次的成本
/// 远高于在这里构造。
pub struct PlaybackVoice {
    consumer: AudioConsumer,
    control: Arc<PlaybackControl>,
    /// 当前增益。回调本地，不在原子量里——每样本读一次原子量是没有必要的
    /// 开销，而且会让「这一个样本用的是哪个增益」变得说不清。
    gain: f64,
    /// 每个样本的增益步长。
    step: f64,
    was_playing: bool,
    /// 本次回放已经渲染出过东西了。用来区分「起播第一块还没等到波形」
    /// 和「播到一半断粮」——前者是正常的，后者要计数。
    produced_any: bool,
    /// 连续多少个块没有拿到新样本。
    starved_blocks: u32,
    /// 引擎没能兑现承诺，正在走淡出收尾。见 `render_mono`。
    aborting: bool,
}

impl PlaybackVoice {
    /// `sample_rate` 必须是**实际达成**的那个，不是请求的那个。
    pub fn new(mut consumer: AudioConsumer, control: Arc<PlaybackControl>, sample_rate: f64) -> Self {
        // 先倒掉建流时可能残留的东西。环是新建的，这一步通常什么也不做，
        // 但「通常」不是「必然」。
        let mut scratch = [0.0f32; 256];
        while consumer.pop_into(&mut scratch) == scratch.len() {}

        let sr = if sample_rate.is_finite() && sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        Self {
            consumer,
            control,
            gain: 0.0,
            step: 1.0 / (sr * FADE_OUT_S).max(1.0),
            was_playing: false,
            produced_any: false,
            starved_blocks: 0,
            aborting: false,
        }
    }

    /// 当前增益，供测试断言用。
    pub fn gain(&self) -> f64 {
        self.gain
    }

    /// 渲染一个块。`out` 是要填的单声道缓冲区。
    ///
    /// 返回实际写入的样本数；**`out` 的剩余部分不被触碰**——cpal 的输出
    /// 缓冲区已经预填了静音，所以「不写」就等于「静音」，而多此一举地
    /// 去写零只是浪费一次内存带宽。
    ///
    /// 绝不分配、绝不加锁、绝不阻塞。
    pub fn render_mono(&mut self, out: &mut [f32]) -> usize {
        if !self.control.playing.load(Ordering::Relaxed) {
            self.was_playing = false;
            self.produced_any = false;
            // 清掉残波。见模块文档：这是「先置 playing 再压入」这个顺序
            // 存在的理由。
            self.drain();
            self.control.idle.store(true, Ordering::Release);
            return 0;
        }

        // 急停和「引擎违约」走同一条淡出路径——两者都是「在不该停的
        // 地方停下」，而停下就必须淡出。
        let stopping = self.control.stopping.load(Ordering::Relaxed) || self.aborting;
        let target = if stopping {
            0.0
        } else {
            self.control.gain_linear()
        };

        if !self.was_playing {
            // 起播。**不做淡入**，理由见模块文档。
            // 这里之所以安全，是因为波形第一个样本恰好是 0。
            self.gain = target;
            self.was_playing = true;
            self.produced_any = false;
            self.starved_blocks = 0;
            self.aborting = false;
        }

        let expected = self.control.expected_samples();
        let n = self.consumer.pop_into(out);

        if n == 0 && !self.produced_any {
            // 起播后的第一块还没等到波形。这是**正常**的：引擎先置
            // `playing` 再压入（见模块文档），中间的窗口是微秒级，但
            // 回调完全可能正好落在那里面。记成欠载的话每次回放都会带上
            // 一个假计数，而这个计数器一旦习惯性非零就再也起不到报警
            // 作用了。
        } else if n == 0 {
            // 播到一半断粮。
            self.starved_blocks = self.starved_blocks.saturating_add(1);
            self.control.underruns.fetch_add(1, Ordering::Relaxed);
            if self.starved_blocks > MAX_STARVED_BLOCKS {
                // 引擎承诺了 `expected` 个却一直没送来。不直接收工——
                // 那样波形会停在一个任意相位上，而那一声咔正是我们要
                // 避免的。改成走淡出。
                self.aborting = true;
            }
        } else {
            self.produced_any = true;
        }

        // 斜坡按**块长**推进，不是按渲染出的样本数推进。
        //
        // 差别在断粮时才显出来：那时 n == 0，按 n 推进的话增益就停在
        // 原地，淡出永远走不完，`playing` 永远为真，半双工永远不解除
        // 静音——**用户看到的是一个麦克风一切正常但完全聋掉的软件**。
        // 而增益本来就该随时间走：8 ms 的淡出是 8 ms 的墙钟，
        // 不是 8 ms 的波形。
        let step = self.step;
        let mut g = self.gain;
        let mut peak = 0.0f32;
        for (i, v) in out.iter_mut().enumerate() {
            let d = target - g;
            g = if d.abs() <= step {
                target
            } else {
                g + step.copysign(d)
            };
            if i >= n {
                // 这一格没有样本要渲染，但时间照样在走。
                continue;
            }
            let y = (*v as f64 * g) as f32;
            *v = y;
            let a = y.abs();
            if a > peak {
                peak = a;
            }
        }
        self.gain = g;
        if n > 0 {
            self.control
                .rendered
                .fetch_add(n as u64, Ordering::Relaxed);
            // 非负 f32 的位模式是单调的，所以整数 max 就是浮点 max。
            self.control
                .peak_bits
                .fetch_max(peak.to_bits(), Ordering::Relaxed);
        }

        // 收工判据是**承诺的样本数**，不是「环空了」。
        //
        // 用「环空了」在 99% 的情况下等价，但那 1% 正是要命的：引擎少
        // 推了一个样本，环就空了，而波形停在一个非零的相位上——一次
        // 满幅阶跃，从用户的大音响里出来是一声清脆的咔。承诺值让这里
        // 能把「放完了」和「没给够」分开。
        if expected > 0 && self.control.rendered.load(Ordering::Relaxed) >= expected {
            // 自然播完（或淡出到一半时正好够了）。波形以恰好 0 结尾，
            // 所以直接收工不会有台阶。
            self.finish(stopping);
        } else if stopping && self.gain <= GAIN_EPSILON {
            // 淡出到底了，剩下的波形不要了。
            self.drain();
            self.finish(true);
        }

        n
    }

    /// 收工。
    fn finish(&mut self, stopped: bool) {
        self.control.playing.store(false, Ordering::Release);
        self.control.stopping.store(false, Ordering::Relaxed);
        self.control.idle.store(true, Ordering::Release);
        if !stopped {
            self.control.finished.store(true, Ordering::Release);
        }
        self.was_playing = false;
        self.produced_any = false;
        self.starved_blocks = 0;
        self.aborting = false;
        // 归零，好让下一次起播的「跃迁」判定干净。反正 `begin` 起播时
        // 会直接跳到目标增益，这个值只影响下一次的第一块。
        self.gain = 0.0;
    }

    /// 清空环里的一切。
    ///
    /// 用栈上的定长块，**不分配**。通常跑一次就退出（正常路径上环在
    /// `finish` 时已经空了），只有手动中止这种情况才可能转上几百圈。
    fn drain(&mut self) {
        let mut buf = [0.0f32; 256];
        loop {
            let n = self.consumer.pop_into(&mut buf);
            if n < buf.len() {
                return;
            }
        }
    }
}

/// 建输出流。
///
/// 返回的 `Stream` **必须由调用线程持有**——cpal 的 `Stream` 不保证
/// `Send`（见 `engine.rs` 的说明），跨线程移动会编译失败或者在某些后端
/// 上运行时炸。
pub struct OutputBuild {
    pub stream: cpal::Stream,
    pub producer: AudioProducer,
    pub control: Arc<PlaybackControl>,
    pub info: StreamInfo,
    /// 设备报上来的错误。引擎每一轮都要把它排空，否则通道满之后
    /// 新错误只会进 `error_counters.dropped` 而不会被任何人看见。
    pub errors: crossbeam_channel::Receiver<cpal::Error>,
    pub error_counters: Arc<ErrorCounters>,
}

impl OutputBuild {
    /// 启动输出流。理由同 [`crate::input::InputBuild::play`]。
    pub fn play(&mut self) -> Result<(), String> {
        use cpal::traits::StreamTrait;
        self.stream
            .play()
            .map_err(|e| format!("启动输出流失败：{e}"))
    }
}

/// 按协商结果建输出流，并把单声道反击声复制到所有声道。
///
/// **复制到所有声道而不是只写第一声道**：用户的大音响接在哪一路是不
/// 确定的，而「反击没声音」最常见的误诊就是接到了右声道而软件只写了左。
/// 代价是立体声音响上会听到两次同一个单声道信号——相位完全一致，
/// 所以既不会抵消也不会变响（除非两个喇叭离得近，那反而更好）。
pub fn build_output_stream(
    device: &cpal::Device,
    sample_rate: Option<u32>,
    ring_capacity_samples: usize,
) -> Result<OutputBuild, noise_core::CoreError> {
    let neg: Negotiated = negotiate(device, false, sample_rate, None)?;

    let (producer, consumer) = AudioProducer::new(ring_capacity_samples);
    let control = Arc::new(PlaybackControl::new());
    let (sink, errors, error_counters) = ErrorSink::new();

    let voice = PlaybackVoice::new(
        consumer,
        Arc::clone(&control),
        neg.config.sample_rate as f64,
    );
    let scratch = vec![0.0f32; DEFAULT_CAPACITY_FRAMES];

    let stream = dispatch_output(device, &neg, voice, scratch, sink)?;

    Ok(OutputBuild {
        stream,
        producer,
        control,
        info: StreamInfo::new(&neg.config, neg.format),
        errors,
        error_counters,
    })
}

/// 12 种数值格式的分派。
///
/// 写成一个宏而不是在 `match` 里重复 12 遍闭包，是因为闭包捕获的东西
/// 必须**按值移动**：`voice` 和 `scratch` 各自只能移动一次，而 `match`
/// 的每个分支都是「移动同一个值」的候选，借用检查器会把它们全部拒绝。
/// 宏展开出的是一串互不相同的表达式，同一时刻只有一条被求值。
macro_rules! build_typed {
    ($device:expr, $config:expr, $ch:expr, $voice:expr, $scratch:expr, $sink:expr, $t:ty) => {{
        let mut voice = $voice;
        let mut scratch = $scratch;
        let sink = $sink;
        let ch = $ch;
        $device.build_output_stream::<$t, _, _>(
            $config,
            move |out: &mut [$t], _: &cpal::OutputCallbackInfo| {
                // 回调边界。debug + rt-guard 构建下，这里之后再分配
                // 就会被记账（见 `rtguard`）。
                let _rt = RtGuard::enter();

                let frames = out.len() / ch;
                let cap = frames.min(scratch.len());
                let n = voice.render_mono(&mut scratch[..cap]);

                // 只写渲染出来的那些帧。剩下的保持 cpal 预填的静音——
                // 再写一遍零只是浪费内存带宽。
                for i in 0..n {
                    let v = <$t as FromSample<f32>>::from_sample_(scratch[i]);
                    let base = i * ch;
                    for c in 0..ch {
                        out[base + c] = v;
                    }
                }
            },
            move |err| sink.report(err),
            None,
        )
    }};
}

fn dispatch_output(
    device: &cpal::Device,
    neg: &Negotiated,
    voice: PlaybackVoice,
    scratch: Vec<f32>,
    sink: ErrorSink,
) -> Result<cpal::Stream, noise_core::CoreError> {
    let config = neg.config.clone();
    // 声道数取一次。闭包里要用，而 `config` 本身已经被移进
    // `build_output_stream` 了。
    let channels = config.channels as usize;

    // 报错信息里要有设备名——「无法使用设备：输出流建立失败」和
    // 「无法使用设备 扬声器 (Realtek)：输出流建立失败」的可用性差很多。
    let name = device_name(device);

    let stream = match neg.format {
        SampleFormat::I8 => build_typed!(device, config, channels, voice, scratch, sink, i8),
        SampleFormat::I16 => build_typed!(device, config, channels, voice, scratch, sink, i16),
        SampleFormat::I24 => build_typed!(device, config, channels, voice, scratch, sink, cpal::I24),
        SampleFormat::I32 => build_typed!(device, config, channels, voice, scratch, sink, i32),
        SampleFormat::I64 => build_typed!(device, config, channels, voice, scratch, sink, i64),
        SampleFormat::U8 => build_typed!(device, config, channels, voice, scratch, sink, u8),
        SampleFormat::U16 => build_typed!(device, config, channels, voice, scratch, sink, u16),
        SampleFormat::U24 => build_typed!(device, config, channels, voice, scratch, sink, cpal::U24),
        SampleFormat::U32 => build_typed!(device, config, channels, voice, scratch, sink, u32),
        SampleFormat::U64 => build_typed!(device, config, channels, voice, scratch, sink, u64),
        SampleFormat::F32 => build_typed!(device, config, channels, voice, scratch, sink, f32),
        SampleFormat::F64 => build_typed!(device, config, channels, voice, scratch, sink, f64),
        // DSD 与 cpal 将来新增的变体走不到这里：`negotiate` 已经用
        // `is_capturable` 把它们挡在协商之外。真走到这里说明协商被绕过
        // 了，那是个 bug，但不能 panic——用户点「开始监测」之后崩掉
        // 是最糟的失败方式。
        other => {
            return Err(noise_core::CoreError::DeviceUnavailable {
                device: name.clone(),
                reason: format!("不支持的采样格式 {other:?}"),
            })
        }
    }
    .map_err(|e: cpal::Error| noise_core::CoreError::DeviceUnavailable {
        device: name.clone(),
        reason: describe_cpal_error(&e),
    })?;

    Ok(stream)
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::{build_thump, ThumpSpec};

    /// 建一个「正在播」的 voice，返回它和环的生产端。
    fn playing(
        samples: &[f32],
        sample_rate: f64,
        gain_db: f64,
        capacity: usize,
    ) -> (PlaybackVoice, AudioProducer, Arc<PlaybackControl>) {
        let (mut p, c) = AudioProducer::new(capacity);
        let control = Arc::new(PlaybackControl::new());
        let voice = PlaybackVoice::new(c, Arc::clone(&control), sample_rate);
        let plan = control
            .begin(gain_db, samples.len() as u64)
            .expect("空闲时应能起播");
        assert!(
            (plan.applied_db - gain_db.clamp(MIN_GAIN_DB, MAX_GAIN_DB)).abs() < 1e-9,
            "实得 {} dB，要求 {gain_db} dB",
            plan.applied_db
        );
        p.push_partial(samples);
        (voice, p, control)
    }

    fn render_all(voice: &mut PlaybackVoice, block: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; block];
        let mut all = Vec::new();
        for _ in 0..10_000 {
            out.iter_mut().for_each(|v| *v = 0.0);
            let n = voice.render_mono(&mut out);
            all.extend_from_slice(&out[..n]);
            if !voice.control.is_playing() && n == 0 {
                break;
            }
        }
        all
    }

    /// 空闲时一个样本都不写，并且不碰输出缓冲——cpal 已经预填了静音，
    /// 再写一遍零只是浪费内存带宽。
    #[test]
    fn an_idle_voice_writes_nothing() {
        let (_p, c) = AudioProducer::new(64);
        let control = Arc::new(PlaybackControl::new());
        let mut voice = PlaybackVoice::new(c, Arc::clone(&control), 48_000.0);

        let mut out = vec![f32::NAN; 4];
        assert_eq!(voice.render_mono(&mut out), 0);
        assert!(out.iter().all(|v| v.is_nan()), "不该触碰输出缓冲");
        assert!(control.is_idle());
    }

    /// 单位增益下样本原样通过（只差 f32→f64→f32 的一次往返）。
    #[test]
    fn a_waveform_passes_through_unit_gain_unchanged() {
        let data: Vec<f32> = (0..512).map(|i| ((i as f32) * 0.01).sin()).collect();
        let (mut voice, _p, _c) = playing(&data, 48_000.0, 0.0, 4096);
        let got = render_all(&mut voice, 64);
        assert_eq!(got.len(), data.len());
        for (i, (a, b)) in got.iter().zip(data.iter()).enumerate() {
            assert!((a - b).abs() < 1e-6, "第 {i} 个样本：{a} vs {b}");
        }
    }

    /// 增益按 dB 生效。−6 dB 是 0.5012，不是 0.5——钉住换算，
    /// 免得有人「顺手」把 `20*log10` 改成 `10*log10`。
    #[test]
    fn gain_is_applied_in_decibels() {
        let data = vec![1.0f32; 256];
        let (mut voice, _p, _c) = playing(&data, 48_000.0, -6.0, 4096);
        let got = render_all(&mut voice, 64);
        let want = 10f64.powf(-6.0 / 20.0) as f32;
        assert!(
            (got[0] - want).abs() < 1e-5,
            "−6 dB 应为 {want}，实得 {}",
            got[0]
        );
    }

    /// **起播不做淡入。**
    ///
    /// 这是本模块最容易写错的一条：加个「平滑起播」看起来永远是正确的，
    /// 实际上会把 8 ms 的升余弦起音抹平成听不出冲击的一声闷响。
    #[test]
    fn starting_snaps_to_gain_and_softens_nothing() {
        // 从**非零**开始。波形自己以 0 起音（`response.rs` 的契约），
        // 所以若照抄那个形状，这条测试就分不出「没淡入」和「淡入了但
        // 起点本来就是 0」。
        let data: Vec<f32> = (1..=64).map(|i| i as f32 / 64.0).collect();
        let (mut voice, _p, _c) = playing(&data, 48_000.0, 0.0, 4096);
        let got = render_all(&mut voice, 16);
        // 第一个样本必须**按单位增益**输出。淡入的话它会是 0 或者很小。
        assert!(
            got[0] > 0.0,
            "起播第一块就被淡入吃掉了，实得 {}",
            got[0]
        );
        assert!((got[1] - data[1]).abs() < 1e-6, "起音斜坡被改动了");
    }

    /// 增益在播放中途变化要走斜坡，不能跳。
    ///
    /// 判据是**相邻样本的差**：波形本身是 1.0 的常数，所以任何跳变都
    /// 只会来自增益。
    #[test]
    fn a_mid_playback_gain_change_ramps_instead_of_jumping() {
        let data = vec![1.0f32; 8192];
        let (mut voice, _p, control) = playing(&data, 48_000.0, 0.0, 16_384);

        let mut out = vec![0.0f32; 128];
        let n = voice.render_mono(&mut out);
        assert_eq!(n, 128);
        let before = out[127];

        // 正在播时第二次 `begin` 被拒——增益不是这样改的，改增益要先停。
        assert!(
            control.begin(-30.0, 8192).is_none(),
            "正在播时不该允许第二次起播"
        );

        // 所以这里用停止淡出来测斜坡本身：它和「播放中改增益」走的是
        // 同一段代码（目标值到 0，按 step 逐样本逼近）。
        control.request_stop();

        let mut prev = before;
        let mut max_jump: f32 = 0.0;
        let mut rendered = 0usize;
        for _ in 0..200 {
            out.iter_mut().for_each(|v| *v = 0.0);
            let n = voice.render_mono(&mut out);
            if n == 0 {
                break;
            }
            for &v in &out[..n] {
                max_jump = max_jump.max((v - prev).abs());
                prev = v;
            }
            rendered += n;
        }
        assert!(rendered > 0, "淡出期间应该还有输出");
        // 步长是 1/(48000*0.008) ≈ 2.6e-3，波形幅度 1.0。
        // 留 4 倍余量给斜坡的最后一格（可能整格落在同一个样本上）。
        assert!(
            max_jump < 0.02,
            "淡出过程中出现了 {max_jump} 的台阶，那就是一声咔"
        );
    }

    /// 停止必须淡出到**恰好** 0，而且是在有限步数内。
    #[test]
    fn stopping_fades_to_exactly_zero_within_the_declared_time() {
        let data = vec![1.0f32; 48_000];
        let sr = 48_000.0;
        let (mut voice, _p, control) = playing(&data, sr, 0.0, 96_000);

        let mut out = vec![0.0f32; 64];
        voice.render_mono(&mut out);
        control.request_stop();

        let mut prev = out[63];
        let mut fade_samples = 0usize;
        loop {
            out.iter_mut().for_each(|v| *v = 0.0);
            let n = voice.render_mono(&mut out);
            if n == 0 {
                break;
            }
            for &v in &out[..n] {
                fade_samples += 1;
                assert!(v <= prev + 1e-6, "淡出不该回升：{prev} → {v}");
                assert!(v >= 0.0);
                prev = v;
            }
            assert!(fade_samples < 48_000, "淡出没有收敛");
        }
        assert_eq!(voice.gain(), 0.0, "增益必须归零");
        assert!(control.is_idle(), "淡完之后要回到 idle");
        assert!(
            !control.take_finished(),
            "被打断的回放不能被记成「自然播完」"
        );

        let fade_ms = fade_samples as f64 * 1000.0 / sr;
        assert!(
            fade_ms < FADE_OUT_S * 1000.0 * 2.0,
            "淡出用了 {fade_ms:.1} ms，远超声明的 {:.0} ms",
            FADE_OUT_S * 1000.0
        );
    }

    /// 自然播完：`finished` 置位，`idle` 复位，而且**最后一个样本是 0**。
    #[test]
    fn a_natural_end_reports_finished_and_lands_on_zero() {
        let spec = ThumpSpec {
            sample_rate: 48_000.0,
            ..Default::default()
        };
        let thump = build_thump(&spec);
        let (mut voice, _p, control) = playing(&thump.samples, 48_000.0, 0.0, 96_000);
        let got = render_all(&mut voice, 128);

        assert_eq!(got.len(), thump.samples.len());
        assert!(control.take_finished(), "自然播完要置 finished");
        assert!(control.is_idle());
        assert_eq!(
            *got.last().unwrap(),
            0.0,
            "波形必须以恰好 0 收尾，否则收工就是一次阶跃"
        );
    }

    /// 跨模块契约：`response.rs` 生成的波形首尾都是恰好 0。
    /// 起播不淡入、收工不淡出这两条**都**依赖它。
    #[test]
    fn the_waveform_contract_holds() {
        for sr in [16_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let thump = build_thump(&ThumpSpec {
                sample_rate: sr,
                ..Default::default()
            });
            assert_eq!(*thump.samples.first().unwrap(), 0.0, "{sr} Hz 起音不是 0");
            assert_eq!(*thump.samples.last().unwrap(), 0.0, "{sr} Hz 收尾不是 0");
        }
    }

    /// 播到一半断粮：补静音、计数、但不崩、不错位，而且**不收工**。
    ///
    /// 收工就错了：承诺了 100 个样本只拿到 32 个，此刻的波形停在一个
    /// 非零相位上，收工就是一次满幅阶跃。
    #[test]
    fn a_starved_voice_outputs_silence_and_counts_it() {
        let (mut p, c) = AudioProducer::new(4096);
        let control = Arc::new(PlaybackControl::new());
        let mut voice = PlaybackVoice::new(c, Arc::clone(&control), 48_000.0);
        control.begin(0.0, 100).unwrap();
        p.push_partial(&[0.5f32; 32]);

        let mut out = vec![9.0f32; 128];
        assert_eq!(voice.render_mono(&mut out), 32);
        assert!((out[0] - 0.5).abs() < 1e-6);

        // 第二块：环空了，本次回放已经渲染过东西 → 记一次欠载。
        // **注意**：我们不往 out 里写零，cpal 的缓冲本来就是零。
        assert_eq!(voice.render_mono(&mut out), 0);
        assert_eq!(control.underruns(), 1);
        assert!(!control.is_idle(), "断粮不等于播完——承诺的 100 个还差 68 个");

        // 补齐剩下的，正常收工。
        p.push_partial(&[0.5f32; 68]);
        assert_eq!(voice.render_mono(&mut out), 68);
        assert!(control.is_idle());
        assert_eq!(control.underruns(), 1, "恢复之后不该继续累加");
    }

    /// 引擎一直不兑现承诺：不能永远挂在那里。
    ///
    /// 挂着的后果不是「没声音」，而是 `playing` 恒为真 → 半双工永不
    /// 解除静音 → **用户看到的是一个麦克风正常但完全聋掉的软件**。
    #[test]
    fn a_broken_promise_eventually_fades_out_instead_of_hanging() {
        let (mut p, c) = AudioProducer::new(4096);
        let control = Arc::new(PlaybackControl::new());
        let mut voice = PlaybackVoice::new(c, Arc::clone(&control), 48_000.0);
        control.begin(0.0, 10_000).unwrap();
        p.push_partial(&[1.0f32; 64]);

        let mut out = vec![0.0f32; 64];
        assert_eq!(voice.render_mono(&mut out), 64);

        let mut blocks = 0;
        loop {
            out.iter_mut().for_each(|v| *v = 0.0);
            voice.render_mono(&mut out);
            blocks += 1;
            assert!(blocks < 10_000, "没有收敛");
            if control.is_idle() {
                break;
            }
        }
        assert!(
            blocks <= MAX_STARVED_BLOCKS as usize + 8,
            "用了 {blocks} 块才收工，超出了 {MAX_STARVED_BLOCKS} 块的容忍度"
        );
        assert!(control.underruns() > 0, "违约必须被记下来");
        assert!(!control.take_finished(), "违约不是「自然播完」");
    }

    /// 起播的第一块拿不到波形是**正常**的（引擎先置 `playing` 再压入），
    /// 不能记成 underrun——否则每次回放都会带一个假的欠载计数，
    /// 而这个计数器一旦习惯性非零就再也起不到报警作用了。
    #[test]
    fn the_first_empty_block_is_not_an_underrun() {
        let (mut p, c) = AudioProducer::new(4096);
        let control = Arc::new(PlaybackControl::new());
        let mut voice = PlaybackVoice::new(c, Arc::clone(&control), 48_000.0);
        control.begin(0.0, 64).unwrap();

        let mut out = vec![0.0f32; 128];
        assert_eq!(voice.render_mono(&mut out), 0);
        assert_eq!(control.underruns(), 0, "起播时环还没被填充是正常的");
        assert!(!control.is_idle(), "但也不能就此收工——承诺的还没到");

        // 波形随后到达，照常播放。
        p.push_partial(&[0.25f32; 64]);
        assert_eq!(voice.render_mono(&mut out), 64);
        assert!(control.is_idle());
    }

    /// 空闲时环必须被清空——否则一次被中止的回放会把残波留到下一次，
    /// 表现为「按了反击，先响出上一段的半截」。
    #[test]
    fn going_idle_drains_the_ring() {
        let (mut p, c) = AudioProducer::new(4096);
        let control = Arc::new(PlaybackControl::new());
        let mut voice = PlaybackVoice::new(c, Arc::clone(&control), 48_000.0);

        // 没在播的时候压进去的东西是残留。
        p.push_partial(&[0.7f32; 512]);
        let mut out = vec![0.0f32; 128];
        assert_eq!(voice.render_mono(&mut out), 0);

        // 重新起播：第一块必须是空的（残留已被清掉），第二块才是新波形。
        control.begin(0.0, 16).unwrap();
        p.push_partial(&[0.25f32; 16]);
        assert_eq!(voice.render_mono(&mut out), 16);
        assert!(
            (out[0] - 0.25).abs() < 1e-6,
            "第一块放出来的应该是新波形，实得 {}",
            out[0]
        );
    }

    /// 上一次还在淡出时，新的起播请求被**拒绝**而不是硬插进去。
    #[test]
    fn begin_is_refused_while_the_previous_one_is_fading() {
        let data = vec![1.0f32; 48_000];
        let (mut voice, _p, control) = playing(&data, 48_000.0, 0.0, 96_000);

        let mut out = vec![0.0f32; 64];
        // 先真正播起来。**这一步不能省**：不播的话，回调在第一次渲染时
        // 看到的就已经是 `stopping`，于是直接把增益取成 0 并在同一块里
        // 收工——那测的是「还没开始就结束」，不是「淡出途中」。
        voice.render_mono(&mut out);
        assert!(control.is_playing());
        control.request_stop();
        voice.render_mono(&mut out); // 淡出到一半（需要 384 个样本）

        assert!(!control.is_idle(), "淡出还没走完");
        assert!(
            control.begin(0.0, 10).is_none(),
            "淡出未完成时不该接受起播"
        );

        // 放完就又能起播了。
        loop {
            let n = voice.render_mono(&mut out);
            if n == 0 {
                break;
            }
        }
        assert!(control.begin(0.0, 10).is_some(), "淡出结束后应恢复可用");
    }

    /// 增益被钳到 [`MAX_GAIN_DB`]，而且**钳这件事要能被报出来**。
    ///
    /// 用户把滑块推到 +20 dB 却听不出差别，唯一的解释就在这里；
    /// 静默钳位会让这个 slider 变成一台坏掉的旋钮。
    #[test]
    fn gain_is_clamped_and_the_clamp_is_reported() {
        let c = PlaybackControl::new();
        let plan = c.begin(20.0, 1).unwrap();
        assert!(plan.clamped);
        assert_eq!(plan.applied_db, MAX_GAIN_DB);
        assert_eq!(plan.requested_db, 20.0);
        assert!((c.gain_linear() - 1.0).abs() < 1e-9);

        let c2 = PlaybackControl::new();
        let plan = c2.begin(-200.0, 1).unwrap();
        assert!(plan.clamped);
        assert_eq!(plan.applied_db, MIN_GAIN_DB);

        let c3 = PlaybackControl::new();
        assert!(!c3.begin(-18.0, 1).unwrap().clamped, "正常值不该被标记");
    }

    /// 非有限的增益请求不能变成 `NaN` 增益——那是「大音响全力输出
    /// 一段噪声」和「完全静音」之间的区别。
    #[test]
    fn a_nonsense_gain_is_not_forwarded() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let c = PlaybackControl::new();
            let plan = c.begin(bad, 1).unwrap();
            assert!(plan.applied_db.is_finite(), "{bad} → {}", plan.applied_db);
            assert!(c.gain_linear().is_finite());
            assert!(c.gain_linear() <= 1.0);
        }
        assert_eq!(db_to_linear(f64::NAN), 0.0, "NaN 增益等同静音");
        assert!(db_to_linear(f64::INFINITY).is_finite());
    }

    /// 输出永远不越界。波形已经归一化到 −1 dBFS，而增益上限是 0 dB，
    /// 所以这条在任何组合下都该成立——它是「反击声不会削波」的守卫。
    #[test]
    fn the_output_never_exceeds_unit_magnitude() {
        for sr in [16_000.0, 48_000.0, 96_000.0] {
            for gain_db in [-40.0, -6.0, 0.0, 12.0] {
                let thump = build_thump(&ThumpSpec {
                    sample_rate: sr,
                    ..Default::default()
                });
                let (mut voice, _p, _c) =
                    playing(&thump.samples, sr, gain_db, 1_000_000);
                let got = render_all(&mut voice, 512);
                let peak = got.iter().fold(0.0f32, |m, v| m.max(v.abs()));
                assert!(
                    peak <= 1.0,
                    "{sr} Hz / {gain_db} dB 下峰值 {peak} 越界"
                );
                assert!(got.iter().all(|v| v.is_finite()));
            }
        }
    }

    /// 峰值统计要如实反映输出过的最大值——耦合校准拿它当真值用。
    #[test]
    fn the_peak_reported_matches_what_was_actually_output() {
        let thump = build_thump(&ThumpSpec::default());
        let (mut voice, _p, control) = playing(&thump.samples, 48_000.0, -6.0, 1_000_000);
        let got = render_all(&mut voice, 512);
        let want = got.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(
            (control.peak_linear() as f32 - want).abs() < 1e-6,
            "报出 {} 实测 {want}",
            control.peak_linear()
        );
        assert!((control.peak_dbfs() - 20.0 * (want as f64).log10()).abs() < 1e-6);
    }

    /// 渲染的样本数与环里消耗掉的样本数必须一致——不一致就意味着有
    /// 样本被静默吞掉或重复播放。
    #[test]
    fn rendered_count_matches_the_waveform_length() {
        let thump = build_thump(&ThumpSpec::default());
        let (mut voice, _p, control) = playing(&thump.samples, 48_000.0, -12.0, 1_000_000);
        let got = render_all(&mut voice, 441);
        assert_eq!(got.len(), thump.samples.len());
        assert_eq!(control.rendered_samples(), thump.samples.len() as u64);
    }

    /// 块大小从 1 到远大于波形，行为必须一致。
    ///
    /// 真实设备给 128、480、1024、1920 都有可能，而不同后端在同一台
    /// 机器上给的不一样——只测一种块大小等于没测边界。
    #[test]
    fn any_block_size_produces_the_same_waveform() {
        let data: Vec<f32> = (0..2000).map(|i| ((i as f32) * 0.03).sin()).collect();
        for block in [1usize, 7, 64, 480, 1024, 1920, 4096] {
            let (mut voice, _p, _c) = playing(&data, 48_000.0, 0.0, 8192);
            let got = render_all(&mut voice, block);
            assert_eq!(got.len(), data.len(), "块大小 {block}");
            for (i, (a, b)) in got.iter().zip(data.iter()).enumerate() {
                assert!((a - b).abs() < 1e-6, "块大小 {block}，第 {i} 个样本");
            }
        }
    }

    /// 输出缓冲比可渲染的多时，多出来的部分不能被写坏。
    #[test]
    fn unwritten_tail_of_the_block_is_left_alone() {
        let (mut p, c) = AudioProducer::new(64);
        let control = Arc::new(PlaybackControl::new());
        let mut voice = PlaybackVoice::new(c, Arc::clone(&control), 48_000.0);
        control.begin(0.0, 8).unwrap();
        p.push_partial(&[0.5f32; 8]);

        let mut out = vec![f32::NAN; 16];
        assert_eq!(voice.render_mono(&mut out), 8);
        assert!(out[..8].iter().all(|v| v.is_finite()));
        assert!(
            out[8..].iter().all(|v| v.is_nan()),
            "没渲染到的部分不该被触碰"
        );
    }

    /// 采样率非法时不能算出 `NaN` 步长——那会让增益永远停不住。
    #[test]
    fn a_nonsense_sample_rate_still_produces_a_usable_step() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let (_p, c) = AudioProducer::new(64);
            let control = Arc::new(PlaybackControl::new());
            let voice = PlaybackVoice::new(c, Arc::clone(&control), bad);
            assert!(
                voice.step.is_finite() && voice.step > 0.0,
                "{bad} → 步长 {}",
                voice.step
            );
        }
    }

    /// 反复起播/停止不出错，而且每次都干净收尾。
    #[test]
    fn repeated_play_cycles_stay_consistent() {
        let thump = build_thump(&ThumpSpec::default());
        let (mut p, c) = AudioProducer::new(1_000_000);
        let control = Arc::new(PlaybackControl::new());
        let mut voice = PlaybackVoice::new(c, Arc::clone(&control), 48_000.0);

        for round in 0..8 {
            if round % 2 == 0 {
                // 自然播完
                control.begin(-20.0, thump.samples.len() as u64).unwrap();
                p.push_partial(&thump.samples);
                let got = render_all(&mut voice, 480);
                assert_eq!(got.len(), thump.samples.len(), "第 {round} 轮");
                assert!(control.take_finished());
            } else {
                // 中途急停
                control.begin(-20.0, thump.samples.len() as u64).unwrap();
                p.push_partial(&thump.samples);
                let mut out = vec![0.0f32; 480];
                voice.render_mono(&mut out);
                control.request_stop();
                loop {
                    if voice.render_mono(&mut out) == 0 {
                        break;
                    }
                }
                assert!(!control.take_finished(), "第 {round} 轮不该记成自然播完");
            }
            assert!(control.is_idle(), "第 {round} 轮结束后应回到 idle");
            assert_eq!(control.starts(), round + 1);
        }
    }
}
