//! 触发状态机。
//!
//! 输入是 [`FrameFeatures`] 流，输出是**边沿触发**的 [`Action`] 流。
//! 这是本 crate 的核心，也是唯一有状态的部分。
//!
//! # 判据为什么是这些
//!
//! 计划里原本的四路合取里，**帧内峰值因数已被实测证伪**：10 ms 帧装不下
//! 63 Hz 的半个周期（周期 16 ms），峰值因数退化成「这一段落在正弦的哪个
//! 相位上」，稳态隆隆声反而比 10 ms 冲击更「陡」。数据见
//! `docs/detector-tuning.md` 第一节。现在的主判据是**帧间上升沿**，
//! 它对同一组信号的区分度是峰值因数的 100 倍。
//!
//! 现在的判据：
//!
//! | 判据 | 作用 | 默认 |
//! |---|---|---|
//! | `trigger_db >= start_db` | 电平，迟滞上阈 | 开 |
//! | `rise >= min_rise_db` | **主判据**，冲击的时域签名 | 开 |
//! | `63Hz带 − 250/500Hz段 >= min_lf_ratio_db` | 低频突出 | 开 |
//! | `crest >= min_crest_db` | 已证伪，默认关 | 关 |
//!
//! # 冲击 vs 持续：上升沿只是候选
//!
//! 上升沿说明电平**起来了**，不说明它**会落下去**。空调启动、冰箱压缩机
//! 同样陡峭，而它们不是邻居的脚步。所以一次上升只是**候选冲击**，之后
//! 电平衰减了 `min_impact_decay_db` 才确认为 [`Action::Impact`]；到
//! `impact_confirm_ms` 仍不衰减则判为持续源。
//!
//! 这一步直接决定了**反击只对冲击发起**（[`EventKind::warrants_response`]）
//! ——对着一直响的空调反击毫无意义，只会把自己的音响开成新的噪声源。
//!
//! # 时基
//!
//! 全部时间量都取自 `FrameFeatures::t_ms`（帧时基），**不引入第二个时钟**。
//! 帧时基随实际音频推进，是样本准确的：两个时钟一旦并存，它们之间的
//! 漂移就是又一处需要解释的不一致，而证据材料最不需要的就是这个。
//!
//! [`EventKind::warrants_response`]: noise_core::EventKind::warrants_response

use crate::config::DetectorConfig;
use crate::floor::AdaptiveNoiseFloor;
use crate::gate::{GateState, HalfDuplexGate};
use crate::hysteresis::{self, Thresholds};
use noise_core::{EventFlags, EventKind};
use noise_dsp::features::FrameFeatures;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// 状态机当前所处的大状态。
///
/// 参与序列化是因为它会出现在 `monitor://level` / `monitor://status`
/// 的载荷里。用 `snake_case` 而不是默认的变体名：这些字符串会进
/// 前端代码和调试输出，`"suspended"` 比 `"Suspended"` 少一次大小写
/// 转换，也少一次「前端到底该不该转」的争论。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorState {
    /// 无事件进行中。底噪在此态自适应。
    Idle,
    /// 事件进行中。底噪**冻结**。
    Active,
    /// 门控硬静音（正在回放）。底噪冻结，只观测不判定。
    Suspended,
}

/// 上升沿如何被处置——即为什么没有产生事件。
///
/// 只看「有没有事件」无法区分「什么都没发生」和「发生了但被挡住」，
/// 而这两者对调参的含义完全相反。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Suppression {
    /// 门控处于冷却期。
    CoolingDown,
    /// 低频突出度不够。
    LowFrequencyRatio,
    /// 峰值因数不够（该判据默认关闭，所以默认永远不会出现）。
    LowCrest,
}

/// 状态机输出。
///
/// 边沿触发：电平一直很高时不会每帧都发 `Impact`。一帧最多产生一个。
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// 事件开始。时间戳回填到**触发那一刻**，不是确认那一刻。
    EventStarted {
        seq: u64,
        t_ms: f64,
        trigger_db: f64,
        floor_db: f64,
        thresholds: Thresholds,
    },
    /// 冲击**确认**。比上升沿晚 `impact_confirm_ms` 以内。
    ///
    /// 反击由这个动作驱动，不由 [`Action::EventStarted`] 驱动——
    /// 前者才知道这到底是不是一次撞击。
    Impact {
        seq: u64,
        t_ms: f64,
        /// 候选刚出现时的电平。
        onset_db: f64,
        /// 确认期限内的峰值。
        peak_db: f64,
        /// 确认时的衰减量，dB。必然 ≥ `min_impact_decay_db`。
        decay_db: f64,
    },
    /// 事件结束。
    EventEnded(EndedEvent),
    /// 出现了会触发的事件，但被挡下了。同样边沿触发。
    Suppressed {
        reason: Suppression,
        t_ms: f64,
        trigger_db: f64,
    },
}

/// 一个已结束的事件的全部判定结果。
///
/// 调用方负责把它和录到的音频、DSP 的软标记合并成 `EventRecord`。
#[derive(Clone, Debug, PartialEq)]
pub struct EndedEvent {
    pub seq: u64,
    /// 触发时刻（不是确认时刻）。
    pub started_ms: f64,
    /// 结束时刻（电平跌破停止门限后，再过完 hangover）。
    pub ended_ms: f64,
    pub duration_ms: f64,
    /// 已确认的冲击次数。**不含**未通过衰减检验的候选。
    pub impacts: u32,
    /// 被判定为持续源而否决的候选次数。
    pub rejected_candidates: u32,
    /// 事件期间的最高电平，dB。
    pub peak_db: f64,
    /// 结束时的电平，dB。
    pub end_db: f64,
    /// 事件开始时的底噪，dB。
    pub floor_db: f64,
    /// `impacts > 0` 时为 [`EventKind::Impact`]，否则为 [`EventKind::Sustained`]。
    ///
    /// 用「有没有确认的冲击」而不是「峰值减尾电平够不够」来分类，是因为
    /// 后者在有持续背景声（空调）时会把真实的脚步也判成持续——一个
    /// 走着的人配一台开着的空调，不该因此丢掉「有人在走」这个结论。
    pub kind: EventKind,
    /// DSP 侧能给出的软标记。`self_generated` / `degraded_dsp` /
    /// `dropped_audio` 由调用方补。
    pub flags: EventFlags,
}

/// 运行计数器。调参和浸泡测试靠它，不靠日志。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DetectorCounters {
    pub frames: u64,
    pub events_started: u64,
    pub impacts_confirmed: u64,
    /// 候选冲击因「期限内未衰减」被否决的次数。持续源的直接度量。
    pub candidates_rejected: u64,
    pub suppressed_by_gate: u64,
    pub suppressed_by_lf_ratio: u64,
    pub suppressed_by_crest: u64,
    /// 门控硬静音期间收到的帧数（即「自己的回放盖住了耳朵」的时长）。
    pub frames_while_muted: u64,
    /// 门控冷却期间收到的帧数。
    pub frames_while_cooling: u64,
}

/// 进行中的事件。
#[derive(Clone, Debug, PartialEq)]
struct ActiveEvent {
    seq: u64,
    started_frame: u64,
    started_ms: f64,
    /// 最后一次「电平高于停止门限」的时刻，用于 hangover 计时。
    last_above_stop_ms: f64,
    impacts: u32,
    rejected: u32,
    peak_db: f64,
    floor_db: f64,
    started_trigger_db: f64,
    /// 触发时的低频突出度，用于软标记。
    lf_ratio_db: f64,
    /// 触发时的频谱平坦度，用于软标记。
    flatness: f64,
}

/// 已满足电平与上升沿条件、但频谱判据还差一帧才能定的起始。
///
/// # 为什么必须延后一帧
///
/// 八度带滤波器组的群延迟明显长于触发带——触发带是 20–200 Hz 的宽带，
/// 而 63 Hz 带自己还要过 44.5 Hz 高通 + 89 Hz 低通。结果是**突发第一帧
/// 的频谱判据还没跟上**。实测（48 kHz，100 ms 63 Hz 突发，+25 dB）：
///
/// ```text
/// t=3000 ms  trig=-40.0 dB  lf_ratio= 3.56 dB   ← 上升沿在这一帧
/// t=3010 ms  trig=-33.7 dB  lf_ratio=17.40 dB   ← 频谱在这一帧才成立
/// ```
///
/// 按第一帧判定会把**每一次真实脚步都否掉**，而且否掉之后电平已经上去、
/// 不再有上升沿——不会重试。这不是「偶尔漏一个」，是系统性地全漏。
///
/// # 代价与补偿
///
/// 代价是 10 ms 的判定延迟。补偿是：事件时间戳**回填到上升沿那一帧**
/// （`t_ms` 取自本结构），所以下游看到的时序与不延后完全一致，
/// 只有动作的产生时刻晚了 10 ms。
#[derive(Clone, Copy, Debug, PartialEq)]
struct PendingOnset {
    frame_index: u64,
    t_ms: f64,
    db: f64,
    floor_db: f64,
    thresholds: Thresholds,
}

/// 待确认的冲击候选。
#[derive(Clone, Copy, Debug, PartialEq)]
struct PendingImpact {
    onset_ms: f64,
    onset_db: f64,
    peak_db: f64,
    /// 到这个帧号仍未衰减即判为持续源。
    deadline_frame: u64,
}

/// 已看到一次上升、等下一帧确认它**不是单帧毛刺**。
///
/// # 为什么「上升量」本身不够
///
/// 帧级电平估计是有噪声的：窄带稳态源（水泵、压缩机、电机嗡声）在
/// 触发带里只有一两个自由度，帧间起伏的标准差可达 3–4 dB。把「本帧相对
/// 前 50 ms 最大值高出 15 dB」当判据，这样的源在几千帧里必然会蒙中
/// 几次——实测场景 4 的 60 秒空调声（低频主导、稳态）就会确认出一次
/// **冲击**，而冲击会被 `warrants_response()` 放行，直接导致对着一直响的
/// 空调反击。
///
/// # 区别在持续时间，不在上升量
///
/// 一次真实撞击的能量持续几十毫秒（几步、敲击、拖家具），而一次起伏
/// 只有一帧。但这一点**没法靠数「上升沿持续了几帧」来区分**：滑动的
/// 基线窗口会把刚刚那一帧吞进去，于是单帧毛刺和六帧突发都会恰好产生
/// 两个连续的上升沿判定。
///
/// 所以基线必须**在上升的那一刻固定下来**，然后看下一帧是否还站在
/// 上升之后的高位上：
///
/// ```text
///              基线（固定）    第 1 帧     第 2 帧     判定
/// 单帧毛刺        -60          -45  ↑      -60        结束得太快 → 否决
/// 真实撞击        -60          -45  ↑      -42        还在高位   → 确认
/// ```
///
/// 代价是候选确认晚 10 ms，对「反击约 100 ms 准确」的延迟预算无关紧要。
#[derive(Clone, Copy, Debug, PartialEq)]
struct PendingRise {
    frame_index: u64,
    /// 上升之前的窗口最大值。**固定**，不随帧滑动。
    baseline_db: f64,
}

/// 已看到一次衰减、等下一帧确认它**不是单帧缺口**。
///
/// # 这是 [`PendingRise`] 的镜像，理由是同一条
///
/// 一次**持续源**（空调、水泵、车流）的起始阶跃，在触发带的输出上会先
/// 冲过头再凹下去——`start_event` 记下的 `onset_db` 是那个冲高的顶，紧接
/// 着的一帧却掉到远低于它的位置。实测场景 4 的 +25 dB 隆隆声：
///
/// ```text
/// t/ms     -60 帧基准    5010      5020      5030      5040
/// dB     -63.8         -44.0     -44.3     -55.8     -42.0
///                                          ↑ 这一帧
/// ```
///
/// 5030 那一帧相对顶点低了 11.8 dB，越过 `min_impact_decay_db = 10`，
/// 于是**一直响着的空调被确认成了一次冲击**——而冲击会被
/// `warrants_response()` 放行，直接导致对着空调反击。
///
/// # 区别仍然是持续时间
///
/// 真实撞击的能量掉下去以后**不会马上弹回**（下一声脚步之前，房间回到
/// 环境级），而阶跃的凹口只有一帧：5040 就又回到了 -42.0。所以衰减也
/// 要下一帧再确认一次，判据是「**还在低位**」。
///
/// 代价是冲击确认再晚 10 ms，同样在「反击约 100 ms 准确」的预算之内。
#[derive(Clone, Copy, Debug, PartialEq)]
struct PendingDecay {
    frame_index: u64,
    /// 记下这次衰减时的峰值。下一帧要拿它重新比一次。
    peak_db: f64,
}

/// 触发器。
pub struct Detector {
    cfg: DetectorConfig,
    state: DetectorState,
    floor: AdaptiveNoiseFloor,
    /// `None` 表示回放功能关闭（L0）。此时检测路径上**完全没有门控代码**。
    gate: Option<HalfDuplexGate>,
    /// 最近若干帧的电平，用于上升沿判据。最新的在尾部。
    recent_db: VecDeque<f64>,
    active: Option<ActiveEvent>,
    /// 已满足电平与上升沿、等待频谱结算的起始。
    pending_onset: Option<PendingOnset>,
    /// 已看到上升、等待下一帧确认它不是单帧毛刺。
    pending_rise: Option<PendingRise>,
    /// 已看到衰减、等待下一帧确认它不是单帧缺口。
    pending_decay: Option<PendingDecay>,
    pending: Option<PendingImpact>,
    next_seq: u64,
    counters: DetectorCounters,
    thresholds: Thresholds,
}

impl Detector {
    /// 回放关闭（L0）的构造：分析路径上不存在门控。
    pub fn new(cfg: DetectorConfig) -> Self {
        Self::build(cfg, None)
    }

    /// 回放开启的构造。`gate` 由调用方持有并提供。
    pub fn with_gate(cfg: DetectorConfig, gate: HalfDuplexGate) -> Self {
        Self::build(cfg, Some(gate))
    }

    fn build(cfg: DetectorConfig, gate: Option<HalfDuplexGate>) -> Self {
        let floor = AdaptiveNoiseFloor::new(
            cfg.frames_per_second(),
            cfg.floor_tau_up_s,
            cfg.floor_tau_down_s,
        )
        .with_warmup(cfg.floor_warmup_s, cfg.floor_warmup_tau_s);

        // +1：窗口要装下 rise_window_frames 个**上升之前**的帧，再加上
        // 上升那一帧本身（见 `push_recent`）。
        let recent_len = cfg.rise_window_frames + 1;
        Self {
            cfg,
            state: DetectorState::Idle,
            floor,
            gate,
            recent_db: VecDeque::with_capacity(recent_len),
            active: None,
            pending_onset: None,
            pending_rise: None,
            pending_decay: None,
            pending: None,
            next_seq: 0,
            counters: DetectorCounters::default(),
            thresholds: hysteresis::derive(crate::floor::DB_FLOOR, &cfg, None),
        }
    }

    // ---- 只读视图 ----

    pub fn state(&self) -> DetectorState {
        self.state
    }

    pub fn counters(&self) -> DetectorCounters {
        self.counters
    }

    pub fn thresholds(&self) -> Thresholds {
        self.thresholds
    }

    pub fn floor_db(&self, reference: f64) -> f64 {
        self.floor.value_db(reference)
    }

    pub fn floor_is_initialized(&self) -> bool {
        self.floor.is_initialized()
    }

    /// 进行中事件的信息，供 UI 显示「已持续 N 秒、M 次冲击」。
    pub fn active_event(&self) -> Option<(u64, f64, u32)> {
        self.active
            .as_ref()
            .map(|a| (a.seq, a.started_ms, a.impacts))
    }

    pub fn gate(&self) -> Option<&HalfDuplexGate> {
        self.gate.as_ref()
    }

    pub fn gate_mut(&mut self) -> Option<&mut HalfDuplexGate> {
        self.gate.as_mut()
    }

    // ---- 主循环 ----

    /// 处理一帧。返回本帧产生的动作（没有则 `None`）。
    pub fn process(&mut self, f: &FrameFeatures) -> Option<Action> {
        self.counters.frames += 1;
        let t = f.t_ms;
        let db = f.trigger_db();

        // 门控推进必须无条件执行：冷却期的到期只由时间决定，
        // 不能因为「这一帧电平很低所以早退」而永远不解除。
        if let Some(g) = self.gate.as_mut() {
            g.tick(t);
        }

        // 硬静音（正在回放）：只观测，不判定。
        let muted = self.gate.as_ref().is_some_and(|g| g.is_hard_muted());
        if muted {
            self.counters.frames_while_muted += 1;
            let floor_db = self.floor.value_db(f.reference);
            if let Some(g) = self.gate.as_mut() {
                // 这是 L3 实测耦合的数据来源。
                g.observe_self_noise(db, floor_db);
            }
            // 进行中的事件必须在这里收尾。回放期间听不到楼上，事件的后续
            // 既无从判断也不该被编造；留着不收，回放结束后会把两段互不
            // 相关的录音拼成一条记录。
            let ended = if self.active.is_some() {
                self.finish_event(t, db)
            } else {
                None
            };
            self.state = DetectorState::Suspended;
            // 回放期间以及紧随其后的冷却期，底噪一律冻结（L1 + L2）：
            // 否则自己的回放尾音会被学进底噪，阈值被自己抬上去。
            self.floor.freeze();
            return ended;
        }

        // 从硬静音回到正常。**必须显式复位**：不复位的话 `state` 会永远
        // 停在 `Suspended`，而底噪只在 `Idle` 态自适应——一次回放之后
        // 底噪就再也不会跟环境走了，且完全无声无息。
        if self.state == DetectorState::Suspended {
            self.state = DetectorState::Idle;
        }

        let cooling = self
            .gate
            .as_ref()
            .is_some_and(|g| g.state() == GateState::Cooldown);
        if cooling {
            self.counters.frames_while_cooling += 1;
        }

        // ---- 上升沿 ----
        //
        // 必须在 `push_recent` **之前**算：判据要用的是「本帧之前」的窗口。
        let rise_info = self.deferred_rise_db(db);
        let rising = rise_info.is_some_and(|(rise, _)| rise >= self.cfg.min_rise_db);
        let baseline_db = rise_info.map_or(f64::NEG_INFINITY, |(_, base)| base);
        self.push_recent(db);

        // ---- 结算挂起的起始判定 ----
        //
        // 见 `PendingOnset` 的文档：频谱判据要等一帧才成立，而事件
        // 时间戳回填到上升沿那一帧，所以下游看到的时序不受影响。
        if let Some(p) = self.pending_onset {
            if f.frame_index > p.frame_index {
                self.pending_onset = None;
                return Some(self.settle_onset(p, f));
            }
            // 还要再等一帧。这一帧不参与任何判定，也不更新底噪
            // （见下），以免把候选事件自己的能量学进去。
            return None;
        }

        // ---- 事件生命周期 ----

        // 冷却期内不判定。放在事件判定之前，这样冷却期的突发会被
        // 明确记为「被门控挡下」而不是「什么都没发生」。
        let allowed = self.gate.as_ref().is_none_or(|g| g.allows_detection());

        if self.active.is_some() {
            // 事件进行中：先看是否该结束，再看是否是又一次冲击。
            if db >= self.thresholds.stop_db {
                if let Some(a) = self.active.as_mut() {
                    a.last_above_stop_ms = t;
                }
            }
            let expired = self.active.as_ref().is_some_and(|a| {
                t - a.last_above_stop_ms >= self.cfg.hangover_ms
            });
            if expired {
                let ended = self.finish_event(t, db)?;
                // 事件结束的这一帧本身仍可能是一次新上升沿，但停止门限
                // 与起始门限之间隔着死区，同一帧不可能既结束又起始。
                return Some(ended);
            }
        }

        // 底噪只在 Idle 态自适应。Active 期间冻结，否则事件自身的能量
        // 会被学进底噪，长事件会被自己的能量「吃掉」（停止门限跟着涨，
        // 事件永远结束不了）。
        //
        // 但**第 0 帧绝不参与播种**：滤波器从全零状态起步，而 63 Hz 的
        // 周期（16 ms）比帧长（10 ms）还长，头一帧的能量被低估约 7 dB
        // （实测数据见 `offline.rs::first_frame_is_depressed_by_the_filters_zero_initial_state`）。
        // 底噪的第一帧就是初值，用它播种等于把整个会话前几十秒的灵敏度
        // 系统性地抬高 7 dB——接下来什么都会触发，包括房间里本来正常的
        // 背景声。丢掉一帧的代价是 10 ms，换来的是可信的初值。
        let seedable = f.frame_index > 0 || self.floor.is_initialized();
        // 冷却期也冻结（L2）：冷却期录到的正是自己回放的尾音和房间衰减，
        // 学进底噪等于让阈值被自己的反击声抬上去。
        let gate_closed = !allowed;
        let hold_floor = self.active.is_some() || gate_closed;
        if self.state == DetectorState::Idle && seedable && !hold_floor {
            self.floor.update(f.trigger_mean_square, f.reference);
        }
        self.thresholds = hysteresis::derive(
            self.floor.value_db(f.reference),
            &self.cfg,
            self.gate.as_ref().and_then(|g| g.coupling_excess_db()),
        );

        let above_start = db >= self.thresholds.start_db;

        // 候选冲击的推进：无论是否产生动作都要跑，否则一次上升的
        // 确认或否决会永远悬着。
        let mut deferred: Option<Action> = None;

        // 事件内的一次上升沿：`advance_pending` 是**唯一**的开候选的地方。
        // 以前这里还另有一条 `arm_candidate` 分支，它和 `advance_pending`
        // 里的那条在同一帧都会跑（`above_start` 蕴含 `db >= stop_db`），
        // 于是候选被开两遍、确认期被悄悄延长一帧——更重要的是，任何加在
        // `advance_pending` 里的把关都会被这条绕过去。
        if self.active.is_some() {
            if let Some(action) = self.advance_pending(f, db, rising, baseline_db) {
                deferred = Some(action);
            }
        }

        if !above_start || !rising {
            return deferred;
        }

        // 到这里说明「电平够高且出现上升沿」——本该触发。
        //
        // 冷却期在这里直接给出结论，不延后：它是纯粹的时序问题，
        // 与频谱判据需要等滤波器收敛是两回事。
        if !allowed {
            self.counters.suppressed_by_gate += 1;
            return deferred.or(Some(Action::Suppressed {
                reason: Suppression::CoolingDown,
                t_ms: t,
                trigger_db: db,
            }));
        }

        // 事件进行中的上升沿已经在上面交给 `advance_pending` 了。
        if self.active.is_some() {
            return deferred;
        }

        // 事件起始：武装一个候选，下一帧用**收敛后的**频谱结算。
        self.pending_onset = Some(PendingOnset {
            frame_index: f.frame_index,
            t_ms: t,
            db,
            floor_db: self.thresholds.floor_db,
            thresholds: self.thresholds,
        });
        // 底噪立即冻结：这一帧的能量已经判定为候选事件能量。
        self.floor.freeze();
        None
    }

    /// 用收敛后的频谱判据结算一个候选起始。
    fn settle_onset(&mut self, p: PendingOnset, f: &FrameFeatures) -> Action {
        match self.spectral_verdict(f) {
            Ok(()) => self.start_event(p, f),
            Err(reason) => {
                // 没有起始就没有事件，底噪解冻恢复跟踪。
                self.floor.unfreeze();
                match reason {
                    Suppression::CoolingDown => self.counters.suppressed_by_gate += 1,
                    Suppression::LowFrequencyRatio => self.counters.suppressed_by_lf_ratio += 1,
                    Suppression::LowCrest => self.counters.suppressed_by_crest += 1,
                }
                Action::Suppressed {
                    reason,
                    // 时间戳用的是**上升沿那一帧**，不是结算帧。
                    t_ms: p.t_ms,
                    trigger_db: p.db,
                }
            }
        }
    }

    /// 频谱类判据。在上升沿的**下一帧**调用，此时八度带已收敛。
    fn spectral_verdict(&self, f: &FrameFeatures) -> Result<(), Suppression> {
        if !self.gate.as_ref().is_none_or(|g| g.allows_detection()) {
            return Err(Suppression::CoolingDown);
        }
        if let Some(min) = self.cfg.min_lf_ratio_db {
            if f.lf_ratio_db() < min {
                return Err(Suppression::LowFrequencyRatio);
            }
        }
        if let Some(min) = self.cfg.min_crest_db {
            if min > 0.0 && f.crest_db < min {
                return Err(Suppression::LowCrest);
            }
        }
        Ok(())
    }

    /// 把一个候选推进一帧，返回确认或否决的动作。
    fn advance_pending(
        &mut self,
        f: &FrameFeatures,
        db: f64,
        rising: bool,
        baseline_db: f64,
    ) -> Option<Action> {
        // ---- 先结算上一帧记下的上升，判定它是不是单帧毛刺 ----
        //
        // 见 `PendingRise`。基线取自上升那一刻，**不随帧滑动**，否则
        // 刚过去的那一帧会被吞进基线里，毛刺和真撞击就分不开了。
        if let Some(r) = self.pending_rise {
            if f.frame_index > r.frame_index {
                self.pending_rise = None;
                if db >= r.baseline_db + self.cfg.min_rise_db
                    && db >= self.thresholds.stop_db
                {
                    self.arm_candidate(f, db);
                }
                return None;
            }
        }

        // ---- 再结算上一帧记下的「落下」，判定它是不是单帧缺口 ----
        //
        // 见 `PendingDecay`。升上去以后那一瞬间的凹口在**单帧**上看起来
        // 和真衰减一模一样。
        //
        // 必须排在「记下新的上升」之前：如果这一帧又弹回高位，它同时
        // 也是一个新的上升沿，而它恰恰就是判定上个缺口的那一帧。排在
        // 后面的话这个上升沿会提前 return，把 `pending_decay` 留到更晚
        // 的帧上去比——那时电平早已无关，比较失去意义。
        if let Some(d) = self.pending_decay {
            if f.frame_index > d.frame_index {
                self.pending_decay = None;
                if d.peak_db - db >= self.cfg.min_impact_decay_db {
                    if let Some(p) = self.pending.take() {
                        if let Some(a) = self.active.as_mut() {
                            a.impacts += 1;
                        }
                        self.counters.impacts_confirmed += 1;
                        return Some(Action::Impact {
                            seq: self.active.as_ref().map_or(0, |a| a.seq),
                            t_ms: f.t_ms,
                            onset_db: p.onset_db,
                            peak_db: d.peak_db,
                            decay_db: d.peak_db - db,
                        });
                    }
                }
                // 又弹回去了：是缺口，不是衰减。候选继续挂着等期限。
            }
        }

        // 事件内出现新的上升沿：先记下来，下一帧再确认。更新的那个才是
        // 真正的起始时刻，旧的那个已经被新能量盖过，它的峰值没有意义。
        if rising && db >= self.thresholds.stop_db {
            self.pending_rise = Some(PendingRise {
                frame_index: f.frame_index,
                baseline_db,
            });
            return None;
        }

        let p = self.pending?;
        let peak = p.peak_db.max(db);

        if peak - db >= self.cfg.min_impact_decay_db {
            // 衰减量达标。**先记下来**，下一帧还在低位才算数。
            self.pending_decay = Some(PendingDecay {
                frame_index: f.frame_index,
                peak_db: peak,
            });
            self.pending = Some(PendingImpact { peak_db: peak, ..p });
            return None;
        }

        if f.frame_index >= p.deadline_frame {
            // 期限到了还在高位 → 持续源，不是冲击。
            self.pending = None;
            self.pending_decay = None;
            if let Some(a) = self.active.as_mut() {
                a.rejected += 1;
            }
            // 全局计数在这里就地递增，而不是等 `finish_event` 结算：
            // 一个持续十分钟的空调事件在它结束之前都该如实地把「否决过
            // 多少次候选」报出来——那是用户判断检测器是不是在瞎起事件
            // 的唯一依据，压在事件末尾等于让它长期显示为 0。
            self.counters.candidates_rejected += 1;
            return None;
        }

        self.pending = Some(PendingImpact { peak_db: peak, ..p });
        None
    }

    fn arm_candidate(&mut self, f: &FrameFeatures, db: f64) {
        let confirm_frames = ((self.cfg.impact_confirm_ms / self.cfg.frame_ms).round() as u64)
            .max(self.cfg.impact_confirm_min_frames as u64);
        self.pending = Some(PendingImpact {
            onset_ms: f.t_ms,
            onset_db: db,
            peak_db: db,
            deadline_frame: f.frame_index + confirm_frames,
        });
        // 新候选不继承上一个候选留下的缺口记录：那个 `peak_db` 属于
        // 另一段能量，拿它跟新候选的低位比会凭空造出一次冲击。
        self.pending_decay = None;
        if let Some(a) = self.active.as_mut() {
            a.peak_db = a.peak_db.max(db);
        }
    }

    /// 正式开始一个事件。
    ///
    /// 时间戳与电平都取自 `p`（上升沿那一帧），频谱软标记取自 `f`
    /// （结算帧，此时才收敛）。这个分工是有意的：**时序要对，频谱也要对**，
    /// 而它们在时间上错开一帧。
    fn start_event(&mut self, p: PendingOnset, f: &FrameFeatures) -> Action {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.counters.events_started += 1;

        self.active = Some(ActiveEvent {
            seq,
            started_frame: p.frame_index,
            started_ms: p.t_ms,
            last_above_stop_ms: p.t_ms,
            impacts: 0,
            rejected: 0,
            peak_db: p.db,
            floor_db: p.floor_db,
            started_trigger_db: p.db,
            lf_ratio_db: f.lf_ratio_db(),
            flatness: f.flatness,
        });
        self.pending = None;
        // 起始事件本身也是一次候选冲击——脚步的第一下同样要衰减才算数。
        // 候选的时基用结算帧，因为「之后有没有衰减」要从现在开始看。
        self.arm_candidate(f, p.db);
        if let Some(a) = self.active.as_mut() {
            a.last_above_stop_ms = p.t_ms;
        }

        self.state = DetectorState::Active;
        self.floor.freeze();

        Action::EventStarted {
            seq,
            t_ms: p.t_ms,
            trigger_db: p.db,
            floor_db: p.floor_db,
            thresholds: p.thresholds,
        }
    }

    /// 结束事件并把状态归位。
    fn finish_event(&mut self, t: f64, db: f64) -> Option<Action> {
        let a = self.active.take()?;
        // 未确认的候选在事件结束时一并否决——没有观察窗口了就无从判断，
        // 记成确认是编造，记成否决是诚实。
        //
        // 注意这里**只**计仍在挂着的那个候选：到期被否决的那些已经在
        // `advance_pending` 里就地计过了（它们在 `a.rejected` 里）。两者
        // 相加才是 `EndedEvent::rejected_candidates`，全局计数器只该各收一次。
        let rejected_now = if self.pending.take().is_some() { 1 } else { 0 };
        self.counters.candidates_rejected += rejected_now as u64;

        let impacts = a.impacts;
        let kind = if impacts > 0 {
            EventKind::Impact
        } else {
            EventKind::Sustained
        };

        let flags = EventFlags {
            // 单频/窄带源（电钻、水泵）的平坦度低。这是软标记，
            // 不做拒绝——空调的平坦度同样低。
            narrowband: a.flatness < 0.3,
            low_frequency_dominant: a.lf_ratio_db > 0.0,
            ..EventFlags::default()
        };

        self.state = DetectorState::Idle;
        self.floor.unfreeze();
        self.recent_db.clear();
        self.pending_onset = None;
        self.pending_rise = None;
        self.pending_decay = None;

        Some(Action::EventEnded(EndedEvent {
            seq: a.seq,
            started_ms: a.started_ms,
            ended_ms: t,
            duration_ms: t - a.started_ms,
            impacts,
            rejected_candidates: a.rejected + rejected_now,
            peak_db: a.peak_db,
            end_db: db,
            floor_db: a.floor_db,
            kind,
            flags,
        }))
    }

    /// 从「后一帧」回看帧 `j` 的上升量是否达标。见 [`Self::deferred_rising`]。
    ///
    /// 窗口本身要能装下 `rise_window_frames` 个**突发之前**的帧，
    /// 再加上突发第一帧——所以容量是 `rise_window_frames + 1`。
    fn push_recent(&mut self, db: f64) {
        self.recent_db.push_back(db);
        while self.recent_db.len() > self.cfg.rise_window_frames + 1 {
            self.recent_db.pop_front();
        }
    }

    /// 上升沿判据，用**下一帧**的电平去量。
    ///
    /// # 为什么不能拿上升沿那一帧自己量
    ///
    /// 分析滤波器在突发之前处理的是安静信号，内部状态接近零；而触发带
    /// 的 20 Hz 高通建立时间是几十毫秒量级。于是**突发第一帧的电平被
    /// 系统性低估约 7 dB**——和会话第 0 帧被低估是同一个机制（见
    /// `offline.rs::first_frame_is_depressed_by_the_filters_zero_initial_state`）。
    ///
    /// 这不是小事：上升沿那一帧恰恰是唯一测不准的一帧，而它正是旧判据
    /// 唯一的依据。实测（场景 7 的 80 ms 63 Hz 突发，稳态高出本底 25.5 dB）：
    ///
    /// ```text
    /// 帧号   电平     旧判据的上升量      拆开看
    ///  299  -60.46        -4.13          本底
    ///  300  -41.76        14.57   ← 突发第 1 帧，被压低 7 dB，判据差 0.43 dB 不达标
    ///  301  -34.30         7.46   ← 基线里已经混进第 300 帧，更不达标
    /// ```
    ///
    /// 结果是**一次都不会触发**，而且不重试：电平已经上去了，不再有新的
    /// 上升沿。这不是「偶尔漏一个」，是系统性全漏。
    ///
    /// # 判据
    ///
    /// ```text
    /// 上升量 = max(上一帧, 本帧) − max(本帧之前的 rise_window_frames 帧)
    ///          ^^^^^^^^^^^^^^^^^   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
    ///          分子取两帧的最大值    分母完全落在突发之前，不受建立过程影响
    /// ```
    ///
    /// 同一次突发：分子 `max(-41.76, -34.30) = -34.30`，分母仍是 −56.33，
    /// 上升量 **22.0 dB** —— 判据通过，而且离真实的 25.5 dB 只差 3.2 dB。
    ///
    /// 剩下的这 3.2 dB 是分母取**最大值**的偏置，属于刻意的保守：取最大
    /// 值意味着「从最近 50 ms 里最响的那一刻算起」，稳态源的正常起伏不会
    /// 被算成上升（取平均则会）。保守的方向是对的，代价是标称的
    /// `min_rise_db` 实际对应约 `min_rise_db + 3` dB 的真实超出量。
    /// 这个偏置随窗口内帧数增长很慢，不需要随配置变化而修正。
    ///
    /// # 代价
    ///
    /// 判定晚一帧（10 ms）。上升沿的**时刻**用本帧，所以事件时间戳不受
    /// 影响；只有动作的产生时刻晚了 10 ms。
    fn deferred_rise_db(&self, now_db: f64) -> Option<(f64, f64)> {
        let n = self.recent_db.len();
        // 头两帧没有「之前」可比。宁可晚几帧触发，也不要在没有依据时
        // 凭空造出一个上升沿。
        if n < 2 {
            return None;
        }
        // 窗口是 [j-n .. j-1]，末位是本帧的前一帧。
        let onset_db = self.recent_db[n - 1].max(now_db);
        let baseline_db = self
            .recent_db
            .iter()
            .take(n - 1)
            .fold(f64::NEG_INFINITY, |m, &v| m.max(v));
        if !baseline_db.is_finite() || !onset_db.is_finite() {
            return None;
        }
        Some((onset_db - baseline_db, baseline_db))
    }

    /// 强制结束进行中的事件（例如用户停止监测、或音频流断开）。
    ///
    /// 与 [`Self::process`] 不同，这里不经过 hangover——调用方已经知道
    /// 「不会再有下一帧了」。
    pub fn force_end(&mut self, t_ms: f64) -> Option<Action> {
        self.active.as_ref()?;
        let db = self
            .recent_db
            .back()
            .copied()
            .unwrap_or(crate::floor::DB_FLOOR);
        self.finish_event(t_ms, db)
    }

    /// 重置到初始状态（换设备、改配置后重启监测）。
    pub fn reset(&mut self) {
        self.state = DetectorState::Idle;
        self.floor.reset();
        self.recent_db.clear();
        self.active = None;
        self.pending_onset = None;
        self.pending = None;
        self.next_seq = 0;
        self.counters = DetectorCounters::default();
    }
}
