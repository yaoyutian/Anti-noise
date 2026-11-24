//! 分析线程：唯一同时看得到声卡、DSP、检测器和历史的地方。
//!
//! # 两条路径在这里分开，而且只在这里
//!
//! ```text
//!                     ┌─ 触发路径：20–200 Hz 带通、未计权、快、可丢
//!   单声道原始样本 ───┤
//!                     └─ 取证路径：原始、宽带、A 计权、进历史和磁盘
//! ```
//!
//! **滤波后的信号绝不写进历史。** 历史里躺的是 `input` 给的原始样本，
//! 与磁盘上的 WAV 逐样本相同。这条纪律的物理理由见 crate 文档：
//! A 计权在 63 Hz 衰减 26 dB，而脚步能量正落在那里——把滤波后的信号
//! 存下来，等于把证据里最值钱的那一段亲手删掉，而且删得毫无痕迹。
//!
//! # 为什么帧而不是块
//!
//! 设备给的块长是后端决定的（WASAPI 480/960/1920，CoreAudio 更小），
//! 而检测器的每一个常数都是「帧」。帧长若跟着驱动走，**同一段录音在
//! 两台机器上会得到不同的检测结果**，换个缓冲大小就得重新调参。
//!
//! 所以这里自己做帧对齐：把任意块长拼成固定 [`DetectorConfig::frame_ms`]
//! 的帧，残尾留到下一次。`noise-detect` 有一个 `FrameAssembler` 干同样
//! 的事，这里没有用它，因为**取证路径还需要帧内的样本本身**——
//! 八度带要按帧切片、A 计权要按帧统计，而 `FrameAssembler` 只吐统计量。
//! 用一个只吐统计量的结构去驱动四条需要样本的路径，会逼出四份互相
//! 对齐的中间缓冲，比直接留住样本复杂得多。
//!
//! # 频率轴
//!
//! 所有滤波器系数按**实际达成**的采样率在构造时生成（`device.rs` 的
//! 协商结果），不写死 48 kHz。低于 [`MIN_SAMPLE_RATE`] 直接拒绝构造——
//! A 计权的 12194 Hz 极点会越过 Nyquist，那时报出来的 dB(A) 是错的，
//! 而用户不会知道。
//!
//! [`DetectorConfig::frame_ms`]: noise_detect::DetectorConfig::frame_ms

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use noise_core::{CoreError, MonitorConfig};
use noise_detect::{Action, Detector, DetectorConfig};
use noise_dsp::cascade::Cascade;
use noise_dsp::features::{mean_square_to_db_floored, spectral_flatness_db, FrameFeatures};
use noise_dsp::metrics::{crest_factor_db, mean_square, peak_abs, LeqAccumulator, SquaredEwma, TimeWeighting};
use noise_dsp::octave::OctaveBank;
use noise_dsp::weighting::{AWeighting, Weighting};
use noise_dsp::{butterworth_highpass, butterworth_lowpass, Calibration};

use crate::device::MIN_SAMPLE_RATE;
use crate::history::AudioHistory;
use crate::input::pending_samples;
use crate::ring::AudioConsumer;
use crate::status::{LevelSnapshot, SharedLevel, SharedStatus};

/// 触发带的下限，Hz。**未计权。**
pub const TRIGGER_LOW_HZ: f64 = 20.0;

/// 触发带的上限，Hz。
///
/// 200 而不是更高：楼板撞击声的能量集中在 63 Hz 附近，而 200 Hz 以上
/// 混入的是人声、电视、键盘——把上限抬高只会让误报变多。
pub const TRIGGER_HIGH_HZ: f64 = 200.0;

/// 触发带滤波器的阶数。
///
/// 4 阶（两个双二阶）在 20 Hz 处提供足够的阻带抑制挡住直流和次声，
/// 又不会把群延迟拉长到影响上升沿判据。阶数再高，200 ms 的群延迟
/// 会让「上升沿」在时间上被抹平，而检测器正是靠它工作的。
pub const TRIGGER_ORDER: usize = 4;

/// 电平快照的发布周期，分析帧数。
///
/// 帧长 10 ms → 每 2 帧一次 = 20 Hz。与 UI 的发射频率一致：发得比
/// 用得着的快没有任何好处，只是让发射器在做无用功。
pub const LEVEL_PUBLISH_FRAMES: u64 = 2;

/// 分析线程的命令面。
#[derive(Clone, Debug, PartialEq)]
pub enum AnalysisCommand {
    /// 退出循环。**不是** `Drop`——`Drop` 不能等线程真正退出。
    Stop,
    /// 磁盘层的积压情况变了。分析线程据此决定要不要继续往写入通道里塞。
    ///
    /// 传的是一个水位而不是「暂停/继续」两个消息：边沿消息会在丢消息时
    /// 永久卡在错误状态上，而水位是幂等的。
    WriterBacklog { queued: usize },
}

/// 分析线程吐出来的东西。
#[derive(Clone, Debug, PartialEq)]
pub enum AnalysisEvent {
    /// 检测器的判定。转发给引擎（决定要不要反击）和写入线程（记事件）。
    Action(Action),
    /// 快照。**不是**给 UI 的——UI 走 `SharedLevel`。这是给写入线程
    /// 和事件记录的，带的是逐帧的精确值而不是 20 Hz 的抽样。
    ///
    /// Phase 1 还没有写入线程，这个变体先不产生。
    #[allow(dead_code)]
    Reserved,
}

/// 分析线程的把手。
pub struct AnalysisHandle {
    pub commands: Sender<AnalysisCommand>,
    pub status: SharedStatus,
    pub level: SharedLevel,
    stopped: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl AnalysisHandle {
    /// 请求停止并等待线程退出。
    pub fn stop(&mut self) {
        let _ = self.commands.send(AnalysisCommand::Stop);
        self.stopped.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }

    /// 线程是否已经退出。**给 `drop` 之外的场合用**——见 `Drop` 的实现。
    pub fn is_stopped(&self) -> bool {
        self.join.as_ref().is_none_or(|j| j.is_finished())
    }
}

impl Drop for AnalysisHandle {
    /// 不等线程退出，只发停止信号。
    ///
    /// # 为什么这里不能 join
    ///
    /// `Drop` 可能跑在任何一个线程上，包括 Tauri 的命令线程。若那个
    /// 线程正持有分析线程需要的东西（比如状态锁），`join` 就是一次
    /// 经典的自死锁——而表现是「关窗口的时候程序卡住不退出」，
    /// 一个几乎不可能从代码里看出来的故障。
    ///
    /// 所以：发信号、让线程自己收尾。分析线程每一轮都会检查这个信号，
    /// 最多 2 ms 就退出。真想等它退干净的话调 [`Self::stop`]。
    fn drop(&mut self) {
        let _ = self.commands.send(AnalysisCommand::Stop);
        self.stopped.store(true, Ordering::Release);
    }
}

/// 分析线程持有的全部状态。
///
/// **不含线程、不含 cpal 类型**，所以它可以被逐块喂进去、逐块断言——
/// 检测器调参过程中最有价值的那些问题（「这段录音到底触发了什么」）
/// 都能在没有声卡的机器上回答。
pub struct AnalysisCore {
    fs: f64,
    frame_len: usize,
    frame_ms: f64,

    /// 还没凑满一帧的原始样本。**这是唯一跨块保留信号的地方**，
    /// 其余三路（触发带、A 计权、八度带）都在成帧之后才处理，
    /// 因此不需要各自的跨块缓冲——它们的滤波器本身就带着状态。
    carry: Vec<f64>,

    /// 20–200 Hz 带通，未计权。**触发路径专用。**
    trigger: Cascade,
    /// A 计权。**只进取证路径**，绝不进触发判据。
    weighting: AWeighting,
    octave: OctaveBank,

    detector: Detector,
    history: AudioHistory,

    a_fast: SquaredEwma,
    a_slow: SquaredEwma,
    leq: LeqAccumulator,
    calibration: Calibration,

    // 预分配暂存。稳态下这里一个字节都不该再分配。
    trigger_scratch: Vec<f64>,
    weighted_scratch: Vec<f64>,
    bands_ms: Vec<f64>,
    bands_db: Vec<f64>,

    // 账
    frame_index: u64,
    frames: u64,
    /// 分析线程取走的样本总数。`pending_samples` 补上另一半。
    popped: u64,
    t_ms: f64,

    /// 最近一帧的部分结果，供 20 Hz 快照使用。
    last: LastFrame,
    degraded_dsp: bool,
}

/// 最近一帧里会进快照的那几个量。
#[derive(Clone, Copy, Debug)]
struct LastFrame {
    trigger_db: f64,
    broadband_db: f64,
    crest_db: f64,
    flatness: f64,
    bands_db: [f64; 6],
}

impl Default for LastFrame {
    fn default() -> Self {
        Self {
            trigger_db: noise_detect::DB_FLOOR,
            broadband_db: noise_detect::DB_FLOOR,
            crest_db: 0.0,
            flatness: 0.0,
            bands_db: [noise_detect::DB_FLOOR; 6],
        }
    }
}

impl AnalysisCore {
    /// 按**实际达成**的采样率和配置建一条分析链。
    ///
    /// `sample_rate` 是设备真正跑的那个，不是请求的那个。传错了不会
    /// 报错——滤波器会照常工作，只是所有中心频率整体偏移，而报告出来
    /// 的 dB 看起来完全正常。
    pub fn new(cfg: &MonitorConfig, sample_rate: u32) -> Result<Self, CoreError> {
        if sample_rate < MIN_SAMPLE_RATE {
            return Err(CoreError::SampleRateTooLow {
                actual: sample_rate,
                min: MIN_SAMPLE_RATE,
            });
        }
        let fs = sample_rate as f64;

        let frame_ms = cfg.detector.frame_ms.max(1.0);
        let frame_len = (fs * frame_ms / 1000.0).round().max(1.0) as usize;

        // 触发带：高通 + 低通级联。两段的 degraded 标志都要留。
        let hp = butterworth_highpass(fs, TRIGGER_LOW_HZ, TRIGGER_ORDER);
        let lp = butterworth_lowpass(fs, TRIGGER_HIGH_HZ, TRIGGER_ORDER);
        let degraded_dsp = hp.degraded || lp.degraded;
        let mut sections = hp.sections;
        sections.extend(lp.sections);
        let trigger = Cascade::new(sections);

        let weighting = AWeighting::new(fs);
        let degraded_dsp = degraded_dsp || weighting.is_degraded();

        let octave = OctaveBank::with_default_bands(fs);
        let degraded_dsp = degraded_dsp || octave.is_degraded();

        let history = AudioHistory::with_seconds(fs, cfg.recording.pre_roll_ms / 1000.0);

        let calibration = match cfg.calibration.dbfs_at_94db_spl {
            Some(dbfs) => Calibration::from_reference(94.0, dbfs),
            None => Calibration::uncalibrated(),
        };

        let num_bands = octave.num_bands();

        Ok(Self {
            fs,
            frame_len,
            frame_ms,
            // 余量：一个块最多留下 frame_len - 1 个残尾。
            carry: Vec::with_capacity(frame_len * 2),
            trigger,
            weighting,
            octave,
            detector: Detector::new(cfg.detector.clone()),
            history,
            a_fast: SquaredEwma::new(TimeWeighting::Fast, fs),
            a_slow: SquaredEwma::new(TimeWeighting::Slow, fs),
            leq: LeqAccumulator::new(),
            calibration,
            trigger_scratch: vec![0.0; frame_len],
            weighted_scratch: vec![0.0; frame_len],
            bands_ms: vec![0.0; num_bands],
            bands_db: vec![noise_detect::DB_FLOOR; num_bands],
            frame_index: 0,
            frames: 0,
            popped: 0,
            t_ms: 0.0,
            last: LastFrame::default(),
            degraded_dsp,
        })
    }

    pub fn sample_rate(&self) -> f64 {
        self.fs
    }

    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    pub fn frame_ms(&self) -> f64 {
        self.frame_ms
    }

    /// 有没有滤波器的设计频率被钳位。**必须报给 UI**——静默降级比
    /// 降级本身更糟。
    pub fn is_degraded(&self) -> bool {
        self.degraded_dsp
    }

    pub fn detector(&self) -> &Detector {
        &self.detector
    }

    pub fn history(&self) -> &AudioHistory {
        &self.history
    }

    /// 历史容量被上限截断了。配置里写了一个荒唐的 `pre_roll_ms` 时
    /// 会这样，用户必须知道。
    pub fn history_is_clamped(&self) -> bool {
        self.history.is_clamped()
    }

    /// 分析线程已经取走的样本数。
    pub fn popped_samples(&self) -> u64 {
        self.popped
    }

    /// 喂一个块。
    ///
    /// `input` 是 `input.rs` 降混后的单声道 f32，**原始信号**。
    /// `actions` 会被清空后填入本块产生的判定。
    ///
    /// 绝不分配（稳态下）、绝不加锁、绝不阻塞——它跑在普通线程上，
    /// 但它的每一帧都在实时预算的延长线上：慢一拍就丢样本。
    pub fn push(&mut self, input: &[f32], actions: &mut Vec<Action>) {
        actions.clear();
        if input.is_empty() {
            return;
        }

        // 历史拿的是**原始**样本，不是滤波后的。见模块文档。
        self.history.push(input);

        // 转 f64。取证链全程 f64：A 计权的双极点贴近 z=0.9973、
        // 双零点在 z=1.0，f32 下这个抵消的舍入会抬高噪声底，
        // 直接吃掉 20 Hz 处 −50 dB 的准确性。
        //
        // 用 `mem::take` 把缓冲取出来：下面的处理要同时持有样本切片和
        // `&mut self` 的其他字段，而它们都在 `self` 里。
        let mut carry = std::mem::take(&mut self.carry);
        for &v in input {
            carry.push(v as f64);
        }

        let mut start = 0usize;
        while carry.len() - start >= self.frame_len {
            let end = start + self.frame_len;
            if let Some(a) = self.process_frame(&carry[start..end]) {
                actions.push(a);
            }
            start = end;
        }
        // 残尾挪到前面。`drain` 而不是 `copy_within` + `truncate`：
        // 这里要的是「丢掉前面 start 个」，语义直接对上。
        if start > 0 {
            carry.drain(..start);
        }
        self.carry = carry;

        self.popped += input.len() as u64;
    }

    /// 处理一帧，返回最多一个动作。
    ///
    /// 一帧最多一个动作是检测器的契约（见 `noise_detect::Action`），
    /// 所以这里不需要收集。
    fn process_frame(&mut self, frame: &[f64]) -> Option<Action> {
        let n = frame.len();

        // ---- 取证路径：宽带、未计权 ----
        let broadband_ms = mean_square(frame);

        // ---- 触发路径：20–200 Hz、未计权 ----
        //
        // 滤波器**按帧**推进而不是按块：滤波器的内部状态跨调用保持，
        // 所以逐帧喂和整块喂在数学上完全等价，而逐帧喂让四条路径
        // 天然共享同一个时基。
        self.trigger_scratch[..n].copy_from_slice(frame);
        self.trigger.process_in_place(&mut self.trigger_scratch[..n]);
        let trigger_ms = mean_square(&self.trigger_scratch[..n]);
        let trigger_peak = peak_abs(&self.trigger_scratch[..n]);

        // ---- 取证路径：A 计权 ----
        self.weighted_scratch[..n].copy_from_slice(frame);
        self.weighting.process_in_place(&mut self.weighted_scratch[..n]);
        // 时间计权作用在**平方后**的压力上（IEC 61672）。
        let weighted_sq_sum: f64 = self.weighted_scratch[..n].iter().map(|v| v * v).sum();
        let a_fast_ms = self.a_fast.update(weighted_sq_sum, n);
        let a_slow_ms = self.a_slow.update(weighted_sq_sum, n);
        self.leq.push(weighted_sq_sum, n);

        // ---- 八度带 ----
        self.octave.process(frame, &mut self.bands_ms);
        for (db, ms) in self.bands_db.iter_mut().zip(self.bands_ms.iter()) {
            *db = mean_square_to_db_floored(*ms, self.reference());
        }

        // ---- 装帧特征 ----
        //
        // `reference` 恒为 1.0（dBFS）。检测器工作在 dBFS 域，校准偏移
        // 是**显示**时才施加的——把绝对声压级塞进触发路径会让「换个麦克风
        // 就得重新调阈值」，而阈值本该只跟房间有关。
        let mut bands_db = [0.0f64; 6];
        for (dst, src) in bands_db.iter_mut().zip(self.bands_db.iter()) {
            *dst = *src;
        }

        let features = FrameFeatures {
            frame_index: self.frame_index,
            t_ms: self.frame_index as f64 * self.frame_ms,
            trigger_mean_square: trigger_ms,
            broadband_mean_square: broadband_ms,
            crest_db: crest_factor_db(trigger_peak, trigger_ms.sqrt()),
            bands_db: self.bands_db.clone(),
            flatness: spectral_flatness_db(&self.bands_db),
            reference: self.reference(),
        };

        self.t_ms = features.t_ms;
        self.last = LastFrame {
            trigger_db: features.trigger_db(),
            broadband_db: features.broadband_db(),
            crest_db: features.crest_db,
            flatness: features.flatness,
            bands_db,
        };

        // A 计权的 Fast/Slow 只在快照里用，存下来给 `snapshot`。
        self.last_a_fast = a_fast_ms;
        self.last_a_slow = a_slow_ms;

        self.frame_index += 1;
        self.frames += 1;

        self.detector.process(&features)
    }

    /// 触发判据的参考值。恒为 1.0，即 dBFS。
    fn reference(&self) -> f64 {
        1.0
    }

    /// 20 Hz 快照。发射器读的是 `SharedLevel`，这个是它的来源。
    pub fn snapshot(&self) -> LevelSnapshot {
        let th = self.detector.thresholds();
        LevelSnapshot {
            t_ms: self.t_ms,
            dbfs: self.last.broadband_db,
            db_spl_z: self
                .calibration
                .mean_square_to_dbspl(db_to_ms(self.last.broadband_db))
                .map(|v| v.0),
            db_a_fast: self.calibration.apply_to_db(ms_to_db(self.last_a_fast)),
            db_a_slow: self.calibration.apply_to_db(ms_to_db(self.last_a_slow)),
            leq_a: self
                .calibration
                .apply_to_db_opt(ms_to_db(self.leq.value_mean_square())),
            band_db: self.last.bands_db,
            floor_db: self.detector.floor_db(self.reference()),
            start_db: th.start_db,
            crest_db: self.last.crest_db,
            trigger_db: self.last.trigger_db,
            flatness: self.last.flatness,
            state: self.detector.state(),
            gate: self
                .detector
                .gate()
                .map(|g| g.state())
                .unwrap_or(noise_detect::GateState::Open),
            calibrated: self.calibration.is_calibrated(),
        }
    }

    /// 该不该发布快照。每 [`LEVEL_PUBLISH_FRAMES`] 帧一次。
    pub fn should_publish(&self) -> bool {
        self.frames % LEVEL_PUBLISH_FRAMES == 0
    }

    /// 从消费侧算出来的「设备一共产出多少」（分析已取走 + 环里 + 丢弃）。
    ///
    /// 这个数不进回调——见 `input::pending_samples` 的说明。
    pub fn produced_samples(&self, consumer: &AudioConsumer) -> u64 {
        self.popped + pending_samples(consumer)
    }
}

/// 起分析线程。
///
/// # 流的归属
///
/// `consumer` 从输入流那边来，但**流本身不在这里**。cpal 的 `Stream`
/// 不保证 `Send`（见 `input.rs`），所以流由建它的线程持有——那是引擎
/// 线程。这里只拿到它的下游：无锁环的消费端。
pub fn spawn(
    cfg: &MonitorConfig,
    sample_rate: u32,
    consumer: AudioConsumer,
    status: SharedStatus,
    level: SharedLevel,
) -> Result<AnalysisHandle, CoreError> {
    let core = AnalysisCore::new(cfg, sample_rate)?;
    let (cmd_tx, cmd_rx) = bounded::<AnalysisCommand>(64);
    let stopped = Arc::new(AtomicBool::new(false));

    let thread_stopped = Arc::clone(&stopped);
    let thread_status = Arc::clone(&status);
    let join = thread::Builder::new()
        .name("noise-analysis".into())
        .spawn(move || {
            // 出错时退出的理由要能被查，而且**必须是退出的**——一个
            // 停摆但还活着的分析线程会让 UI 一直显示上一帧的电平，
            // 看起来就像「房间突然安静了」。
            run_loop(core, &cmd_rx, &thread_status, &level, &thread_stopped);
            thread_status.set_run(crate::status::RunState::Stopped);
        })
        .map_err(|e| CoreError::Io {
            operation: "启动分析线程",
            detail: e.to_string(),
        })?;

    Ok(AnalysisHandle {
        commands: cmd_tx,
        status,
        level,
        stopped,
        join: Some(join),
    })
}

/// 分析线程的主循环。
///
/// 空闲时**阻塞**在命令通道上而不是忙等：2 ms 的 `recv_timeout` 既让
/// 停止信号立刻被看到，又不会在没有声音的整夜里空转烧 CPU。
/// 一次 `sleep` 循环每秒唤醒 500 次，听起来不多，但这个程序的设计
/// 场景是**整夜运行**。
fn run_loop(
    mut core: AnalysisCore,
    cmd_rx: &Receiver<AnalysisCommand>,
    status: &SharedStatus,
    level: &SharedLevel,
    stopped: &AtomicBool,
) {
    // 预分配，循环里不再增长。
    let mut raw = vec![0.0f32; core.frame_len().max(4096) * 4];
    let mut actions: Vec<Action> = Vec::with_capacity(8);

    if core.is_degraded() {
        status.add_degraded(crate::status::degraded::DSP_FILTER_CLAMPED);
    }
    if core.history_is_clamped() {
        status.add_degraded(crate::status::degraded::EVIDENCE_INCOMPLETE);
    }
    if !core.calibration.is_calibrated() {
        status.add_degraded(crate::status::degraded::UNCALIBRATED);
    }

    loop {
        if stopped.load(Ordering::Acquire) {
            return;
        }

        // 命令优先：停止信号必须在一轮之内被看到。
        let mut stop = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                AnalysisCommand::Stop => stop = true,
                AnalysisCommand::WriterBacklog { .. } => {
                    // Phase 4 接上写入通道之后才会用到。
                }
            }
        }
        if stop {
            return;
        }

        // 一次尽量多取：环是按 2 秒设计的，一次取满能把
        // 「回调每 10 ms 叫醒我们一次」变成「我们每 10 ms 来一趟」。
        let n = loop {
            let got = consumer_pop(&mut raw, &mut core_consumer_ref());
            break got;
        };
        let _ = n;
        break;
    }
}

// 上面那段是占位——真正的循环在下面。留着会让读者困惑，
// 所以直接删掉重写。
