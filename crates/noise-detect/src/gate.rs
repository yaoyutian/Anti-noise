//! 半双工回放门控。
//!
//! 麦克风和音响在同一台机器上，回放会被自己的麦克风录到 → 重触发 →
//! 无限循环。这里用**半双工 + 冷却期**阻断它，明确不做 AEC
//! （理由见 `docs/acoustics-notes.md` 第六节）。
//!
//! # 四层防御里这一层负责什么
//!
//! | 层 | 机制 | 防的是 |
//! |---|---|---|
//! | L0 | `playback_enabled=false` 时**根本不构造**本类型 | 根本不进入这条路径 |
//! | L1 | `Muted`：检测器挂起、底噪冻结 | 自己的回放重触发 |
//! | L2 | `Cooldown`：回放后静默 ≥1.5 s + post-roll | 尾音混响、房间衰减 |
//! | L3 | 实测耦合 → 抬高有效起始阈值 | 回放比预期响、换了音响或房间 |
//! | L4 | 速率看门狗：超限即自动停回放 | 任何残留失控 |
//!
//! 本文件实现 L1–L4。L0 由调用方保证——`sm::Detector` 持有的是
//! `Option<HalfDuplexGate>`，关闭回放时是 `None`，检测路径上**可证明**
//! 完全一致（见 `tests/trigger_scenarios.rs::gate_absent_when_playback_disabled`）。

use crate::config::DetectorConfig;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// 门控状态。
///
/// 与 [`crate::sm::DetectorState`] 同理，这个值会进 UI 载荷，
/// 用 `snake_case` 序列化。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateState {
    /// 静默期已过，可以触发、可以回放。
    Open,
    /// 正在回放。**硬静音**：检测器挂起、底噪冻结、帧装配器丢弃残帧。
    Muted,
    /// 回放刚结束的冷却期。仍然不触发，但不丢帧——尾音要照常录进证据。
    Cooldown,
}

/// 请求回放被拒绝的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseRefusal {
    /// 已经在回放中。
    AlreadyPlaying,
    /// 处于冷却期。
    CoolingDown,
    /// L4 速率看门狗触发。
    RateExceeded,
}

/// 半双工回放门控。
#[derive(Clone, Debug)]
pub struct HalfDuplexGate {
    state: GateState,
    /// 当前状态自哪个时刻起（毫秒）。
    since_ms: f64,
    /// 冷却期结束时刻。
    cooldown_until_ms: f64,
    /// 回放发生的时刻（用于 L4 速率看门狗），最近的在尾部。
    response_times_ms: VecDeque<f64>,
    /// Muted 期间观测到的**高出底噪的最大量**，dB。这是 L3 的实测耦合值。
    ///
    /// 刻意存「高出底噪多少」而不是「衰减了多少」：它可以直接和
    /// `start_margin_db` 比较，不需要再做一次换算——而每一次换算
    /// 都是一个可能搞错符号的地方。
    coupling_excess_db: Option<f64>,
    /// L4 是否已触发。触发后门控**永久**保持拒绝，直到显式复位。
    runaway_tripped: bool,
    cfg: DetectorConfig,
}

impl HalfDuplexGate {
    pub fn new(cfg: DetectorConfig) -> Self {
        Self {
            state: GateState::Open,
            since_ms: 0.0,
            cooldown_until_ms: 0.0,
            response_times_ms: VecDeque::new(),
            coupling_excess_db: None,
            runaway_tripped: false,
            cfg,
        }
    }

    pub fn state(&self) -> GateState {
        self.state
    }

    /// 检测路径是否放行。`Muted` 与 `Cooldown` 都不放行。
    pub fn allows_detection(&self) -> bool {
        self.state == GateState::Open
    }

    /// 是否处于硬静音（回放中）。
    ///
    /// 与 `allows_detection` 的区别很重要：`Muted` 期间要丢弃帧装配器的
    /// 残帧（那是自己的回放），`Cooldown` 期间**不能**丢（那是真实环境音，
    /// 要照常录进证据）。
    pub fn is_hard_muted(&self) -> bool {
        self.state == GateState::Muted
    }

    /// 推进时间。冷却期满后自动回到 `Open`。
    pub fn tick(&mut self, now_ms: f64) {
        if self.state == GateState::Cooldown && now_ms >= self.cooldown_until_ms {
            self.state = GateState::Open;
            self.since_ms = now_ms;
        }
    }

    /// 请求开始一次回放。成功则进入 `Muted`。
    ///
    /// **手动「测试回放」也必须走这里**——绕过门控的手动路径正是失控
    /// 最容易钻进来的地方。
    pub fn request_response(&mut self, now_ms: f64) -> Result<(), ResponseRefusal> {
        self.tick(now_ms);

        if self.runaway_tripped {
            return Err(ResponseRefusal::RateExceeded);
        }
        match self.state {
            GateState::Muted => return Err(ResponseRefusal::AlreadyPlaying),
            GateState::Cooldown => return Err(ResponseRefusal::CoolingDown),
            GateState::Open => {}
        }

        // 先把这次的时刻记进环形窗口再判断，这样「第 N 次」本身就是被
        // 判定的一员——否则第 13 次会溜过 12 次的上限。
        self.response_times_ms.push_back(now_ms);
        self.evict_old(now_ms);

        if self.response_times_ms.len() as u32 > self.cfg.max_responses_per_window {
            self.runaway_tripped = true;
            self.response_times_ms.pop_back(); // 这次不算数
            return Err(ResponseRefusal::RateExceeded);
        }

        self.state = GateState::Muted;
        self.since_ms = now_ms;
        self.coupling_excess_db = None; // 本次回放的耦合观测重新开始
        Ok(())
    }

    /// 回放结束，计算并进入冷却期。
    ///
    /// * `response_ms` —— 本次回放波形的时长
    ///
    /// 冷却时长取三者最大：
    ///
    /// ```text
    /// max(cooldown_ms,                     // 用户/测试显式指定的下限
    ///     post_roll_ms + 1500,             // 事件后要录满 + 房间衰减余量
    ///     response_ms  + 500)              // 回放本身 + 尾音
    /// ```
    ///
    /// 第一个是旋钮，后两个是**不变量**：任何配置都不能让冷却短于它们，
    /// 否则尾音会在检测恢复后立刻被当成新事件。
    pub fn finish_response(&mut self, now_ms: f64, response_ms: f64) {
        if self.state != GateState::Muted {
            return;
        }
        let cooldown = self
            .cfg
            .cooldown_ms
            .max(self.cfg.post_roll_ms + 1500.0)
            .max(response_ms + 500.0);
        self.cooldown_until_ms = now_ms + cooldown;
        self.state = GateState::Cooldown;
        self.since_ms = now_ms;
    }

    /// 在 `Muted` 期间观测一帧，记录自己回放造成的电平。
    ///
    /// * `observed_db` —— 该帧的电平
    /// * `floor_db` —— 当时的底噪（已冻结）
    ///
    /// 取整个回放期间的**最大值**而不是平均：阈值要防的是最坏情况，
    /// 平均值会被回放波形里安静的那一段拉低。
    pub fn observe_self_noise(&mut self, observed_db: f64, floor_db: f64) {
        if self.state != GateState::Muted {
            return;
        }
        if !observed_db.is_finite() || !floor_db.is_finite() {
            return;
        }
        let excess = observed_db - floor_db;
        // 记录为正的超出量；负值（回放比底噪还轻）没有意义，钳到 0。
        let excess = excess.max(0.0);
        self.coupling_excess_db = Some(match self.coupling_excess_db {
            Some(prev) => prev.max(excess),
            None => excess,
        });
    }

    /// 实测耦合：自己回放在麦克风处高出底噪多少 dB。
    pub fn coupling_excess_db(&self) -> Option<f64> {
        self.coupling_excess_db
    }

    /// 当前窗口内的回放次数。
    pub fn responses_in_window(&self, now_ms: f64) -> usize {
        let mut n = 0;
        let oldest_allowed = now_ms - self.cfg.rate_window_ms;
        for &t in self.response_times_ms.iter().rev() {
            if t < oldest_allowed {
                break;
            }
            n += 1;
        }
        n
    }

    /// L4 速率看门狗是否已触发。
    pub fn is_runaway(&self) -> bool {
        self.runaway_tripped
    }

    /// 清空速率窗口并解除看门狗。
    ///
    /// **只能由用户显式操作调用**（例如在设置里点「我已检查音响」），
    /// 绝不能被自动重试路径调到——否则看门狗就形同虚设。
    pub fn reset_watchdog(&mut self) {
        self.response_times_ms.clear();
        self.runaway_tripped = false;
    }

    /// 丢弃超出窗口的记录。
    fn evict_old(&mut self, now_ms: f64) {
        let oldest_allowed = now_ms - self.cfg.rate_window_ms;
        while let Some(&front) = self.response_times_ms.front() {
            if front < oldest_allowed {
                self.response_times_ms.pop_front();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
// 同 `config.rs`：测试里「默认值 + 改一项」比抄一遍默认值更不易过时。
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn gate() -> HalfDuplexGate {
        HalfDuplexGate::new(DetectorConfig::default())
    }

    #[test]
    fn starts_open_and_allows_detection() {
        let g = gate();
        assert_eq!(g.state(), GateState::Open);
        assert!(g.allows_detection());
        assert!(!g.is_hard_muted());
    }

    #[test]
    fn response_mutes_immediately() {
        let mut g = gate();
        g.request_response(1000.0).unwrap();
        assert_eq!(g.state(), GateState::Muted);
        assert!(g.is_hard_muted(), "回放期间必须是硬静音");
        assert!(!g.allows_detection());
    }

    #[test]
    fn cooldown_blocks_until_it_expires() {
        let mut g = gate();
        g.request_response(1000.0).unwrap();
        g.finish_response(1400.0, 400.0);

        // 冷却 = max(2000, 3000+1500, 400+500) = 4500
        assert_eq!(g.state(), GateState::Cooldown);
        assert!(!g.allows_detection(), "冷却期不能触发");
        assert!(!g.is_hard_muted(), "冷却期不丢帧——尾音要照常录进证据");

        g.tick(1400.0 + 4499.0);
        assert_eq!(g.state(), GateState::Cooldown);
        g.tick(1400.0 + 4500.0);
        assert_eq!(g.state(), GateState::Open);
        assert!(g.allows_detection());
    }

    /// post-roll 与回放时长的下限**不可**被配置绕过。
    #[test]
    fn cooldown_floors_cannot_be_configured_away() {
        let mut cfg = DetectorConfig::default();
        cfg.cooldown_ms = 10.0; // 用户把它调到荒谬的小值
        cfg.post_roll_ms = 3000.0;

        let mut g = HalfDuplexGate::new(cfg);
        g.request_response(0.0).unwrap();
        g.finish_response(100.0, 800.0);

        // 仍须满足 post_roll + 1500 = 4500
        g.tick(100.0 + 4499.0);
        assert_eq!(g.state(), GateState::Cooldown, "post-roll 下限被绕过了");
        g.tick(100.0 + 4500.0);
        assert_eq!(g.state(), GateState::Open);
    }

    #[test]
    fn long_response_extends_the_cooldown() {
        let mut g = gate();
        g.request_response(0.0).unwrap();
        g.finish_response(20_000.0, 20_000.0); // 一段很长的回放
        g.tick(20_000.0 + 20_499.0);
        assert_eq!(g.state(), GateState::Cooldown);
        g.tick(20_000.0 + 20_500.0);
        assert_eq!(g.state(), GateState::Open);
    }

    #[test]
    fn cannot_start_a_response_while_muted_or_cooling() {
        let mut g = gate();
        g.request_response(0.0).unwrap();
        assert_eq!(g.request_response(100.0), Err(ResponseRefusal::AlreadyPlaying));

        g.finish_response(400.0, 400.0);
        assert_eq!(g.request_response(500.0), Err(ResponseRefusal::CoolingDown));
    }

    #[test]
    fn coupling_observation_uses_the_worst_frame() {
        let mut g = gate();
        g.request_response(0.0).unwrap();
        g.observe_self_noise(-30.0, -60.0); // 高出 30
        g.observe_self_noise(-20.0, -60.0); // 高出 40 —— 最坏
        g.observe_self_noise(-45.0, -60.0); // 高出 15
        assert_eq!(g.coupling_excess_db(), Some(40.0));
    }

    #[test]
    fn coupling_observation_only_happens_while_muted() {
        let mut g = gate();
        g.observe_self_noise(0.0, -60.0);
        assert_eq!(g.coupling_excess_db(), None, "非回放期间不应记录耦合");

        g.request_response(0.0).unwrap();
        g.observe_self_noise(-30.0, -60.0);
        g.finish_response(400.0, 400.0);
        g.observe_self_noise(0.0, -60.0);
        assert_eq!(g.coupling_excess_db(), Some(30.0), "冷却期不应改写耦合值");
    }

    #[test]
    fn playback_quieter_than_floor_clamps_to_zero() {
        let mut g = gate();
        g.request_response(0.0).unwrap();
        g.observe_self_noise(-80.0, -60.0);
        assert_eq!(g.coupling_excess_db(), Some(0.0));
    }

    #[test]
    fn coupling_resets_on_each_new_response() {
        let mut g = gate();
        g.request_response(0.0).unwrap();
        g.observe_self_noise(-20.0, -60.0);
        assert_eq!(g.coupling_excess_db(), Some(40.0));

        g.finish_response(400.0, 400.0);
        g.tick(10_000.0);
        g.request_response(10_000.0).unwrap();
        assert_eq!(g.coupling_excess_db(), None, "新一次回放应重新观测");
    }

    /// 一次回放占用的总时长：波形本身 + 它引发的冷却期。
    ///
    /// 冷却期从 `finish_response` 起算而不是从 `request_response` 起算，
    /// 所以推进量必须是 `response_ms + cooldown`——只推 `cooldown` 会停在
    /// 冷却期中间。这里从配置重算，免得改了 `cooldown_ms` 或 `post_roll_ms`
    /// 之后测试以一个看不懂的方式失败。
    fn cycle_ms(cfg: &DetectorConfig, response_ms: f64) -> f64 {
        let cooldown = cfg
            .cooldown_ms
            .max(cfg.post_roll_ms + 1500.0)
            .max(response_ms + 500.0);
        response_ms + cooldown
    }

    /// L4：超过窗口上限即永久拒绝，不靠自动重试恢复。
    #[test]
    fn rate_watchdog_trips_and_stays_tripped() {
        let cfg = DetectorConfig::default();
        let max = cfg.max_responses_per_window;
        let response_ms = 400.0;
        let mut g = HalfDuplexGate::new(cfg);

        let mut t = 0.0;
        for i in 0..max {
            g.tick(t);
            assert!(
                g.request_response(t).is_ok(),
                "第 {} 次回放不该被拒",
                i + 1
            );
            g.finish_response(t + response_ms, response_ms);
            t += cycle_ms(&cfg, response_ms);
        }

        // 第 max+1 次仍在同一分钟窗口内 → 触发看门狗
        g.tick(t);
        assert_eq!(g.request_response(t), Err(ResponseRefusal::RateExceeded));
        assert!(g.is_runaway());

        // 时间过去很久也不会自动恢复
        g.tick(t + 3_600_000.0);
        assert_eq!(g.request_response(t + 3_600_000.0), Err(ResponseRefusal::RateExceeded));
        assert!(g.is_runaway());
    }

    #[test]
    fn watchdog_can_be_cleared_explicitly() {
        let cfg = DetectorConfig::default();
        let response_ms = 400.0;
        let mut g = HalfDuplexGate::new(cfg);

        let mut t = 0.0;
        for _ in 0..=cfg.max_responses_per_window {
            g.tick(t);
            let _ = g.request_response(t);
            g.finish_response(t + response_ms, response_ms);
            t += cycle_ms(&cfg, response_ms);
        }
        assert!(g.is_runaway());

        g.reset_watchdog();
        assert!(!g.is_runaway());
        g.tick(t);
        assert!(g.request_response(t).is_ok());
    }

    #[test]
    fn old_responses_leave_the_window() {
        let mut g = gate();
        g.request_response(0.0).unwrap();
        g.finish_response(400.0, 400.0);
        assert_eq!(g.responses_in_window(0.0), 1);
        // 一分钟后旧记录应被清出窗口
        assert_eq!(g.responses_in_window(60_001.0), 0);
    }

    #[test]
    fn finish_response_outside_muted_is_a_noop() {
        let mut g = gate();
        g.finish_response(100.0, 400.0);
        assert_eq!(g.state(), GateState::Open, "非回放中结束回放不应改变状态");
    }
}
