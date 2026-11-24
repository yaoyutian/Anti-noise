//! 检测器配置。
//!
//! **默认值全部来自理论推导，不是实测结果。** 63 Hz 峰值、15–40 Hz 楼板
//! 共振、空调底噪、楼板是混凝土还是木结构，都会改变这些数字。用
//! `examples/replay.rs` 拿你自己楼里的录音调，见 `docs/detector-tuning.md`。
//!
//! # 为什么它在 `noise-core` 而不是 `noise-detect`
//!
//! [`crate::MonitorConfig`] 要按字段持有它，而 `noise-detect` 依赖
//! `noise-core`——反过来放就成环了。它本身是纯数据（只依赖 serde），
//! 放在这里没有任何代价。`noise_detect::DetectorConfig` 仍然可用，
//! 那是这里的再导出。

use serde::{Deserialize, Serialize};

/// 检测器的全部可调参数。
///
/// `#[serde(default)]` 是刻意的：新增字段时旧配置文件仍能载入，
/// 缺失的字段取默认值而不是让整个配置反序列化失败。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectorConfig {
    // ---- 分析帧 ----

    /// 分析帧长，毫秒。
    ///
    /// 10 ms 是 IEC 61672 的短时测量惯例，也是本项目的固定时基。
    /// **不要随意改小**：63 Hz 的周期是 16 ms，帧长再短的话帧内连
    /// 半个周期都装不下，各种波形特征会退化成噪声（见
    /// `docs/detector-tuning.md` 第一节的实测数据）。
    pub frame_ms: f64,

    // ---- 自适应底噪 ----

    /// 底噪上升的时间常数，秒。
    pub floor_tau_up_s: f64,

    /// 底噪下降的时间常数，秒。
    ///
    /// 比上升慢一个数量级：房间安静下来的瞬间不应该立刻变得极度敏感，
    /// 否则邻居停下后又开始走动的第一步会被当成巨大冲击。
    pub floor_tau_down_s: f64,

    /// 启动预热时长，秒。预热期间底噪双向都用快时间常数。
    ///
    /// 没有它的话，应用在空调已经开着的房间里启动，底噪会锁在高位然后
    /// 用 300 s 慢慢降下来——用户前五分钟什么都检测不到。
    pub floor_warmup_s: f64,

    /// 预热期间使用的时间常数，秒。
    pub floor_warmup_tau_s: f64,

    // ---- 触发判据 ----

    /// 起始阈值高出底噪多少 dB。
    pub start_margin_db: f64,

    /// 迟滞的下阈值高出底噪多少 dB。必须小于 `start_margin_db`。
    pub stop_margin_db: f64,

    /// **主判据**：帧电平相对前 `rise_window_frames` 帧最大值的最小增量，dB。
    ///
    /// 这是本检测器唯一可靠的判别特征。帧内峰值因数在 10 ms 帧下无法
    /// 区分冲击与稳态噪声，实测数据见 `docs/detector-tuning.md` 第一节。
    pub min_rise_db: f64,

    /// 上升沿的比较窗口，帧数。5 帧 = 50 ms。
    ///
    /// 取 5 帧而不是 1 帧，是因为低频电平的单帧估计本身噪声很大；
    /// 拿前一帧单独比较会被估计噪声触发。
    pub rise_window_frames: usize,

    /// 触发所需的最小低频突出度（63 Hz 带减 250–500 Hz 段），dB。
    ///
    /// 用 `Option` 而不是「某个负数表示关闭」是有意的：`f64::NEG_INFINITY`
    /// 过不了 JSON 一个来回——serde_json 把它写成 `null`，再从 `null`
    /// 反序列化回 `f64` 会失败。一个「关闭」哨兵值把配置文件变成
    /// 一次性用品，而配置文件是要能存下来、改、再读回来的。
    pub min_lf_ratio_db: Option<f64>,

    /// **默认 `None`（关闭）**。见 [`Self::min_rise_db`] 的说明。
    ///
    /// 保留这个旋钮只为实测推翻预测——如果你在自己的录音上发现
    /// 帧内峰值因数确实有判别力，把它设成一个正数并记录数据。
    /// `None` 之外还要求取值 > 0：0 或负数会让判据恒真，等同没写。
    pub min_crest_db: Option<f64>,

    // ---- 冲击确认 ----

    /// 一次「上升」被确认为冲击所需的衰减量，dB。
    ///
    /// 上升沿只说明电平**起来了**，不说明它**会落下去**。空调启动、
    /// 冰箱压缩机启停同样有陡峭的上升沿，而它们不是邻居的脚步。
    /// 区别在于冲击会衰减，持续源不会。
    ///
    /// 所以上升沿只是**候选**：之后若电平衰减了这么多，才确认为冲击。
    /// 这是本检测器区分「冲击」与「持续」的核心，也是
    /// 检测器只在确认之后才发出冲击动作的原因。
    pub min_impact_decay_db: f64,

    /// 候选冲击的确认期限，毫秒。超时仍未见衰减即判为持续源。
    ///
    /// 500 ms 够一次脚步走完「起来—峰值—落下」，又短于一次
    /// 空调启动的持续时间，所以两者在这个尺度上分得开。
    pub impact_confirm_ms: f64,

    /// 冲击确认期限内记录峰值所用的帧步长上限，帧。
    ///
    /// 只是给 `impact_confirm_ms` 的一个下限保护：期限至少要有这么多帧，
    /// 否则单帧的电平估计噪声就足以决定结论。
    pub impact_confirm_min_frames: u32,

    // ---- 事件生命周期 ----

    /// 事件内两次冲击之间的最大间隔，毫秒。超时即结束事件。
    ///
    /// 取值必须**大于你想合并的最慢脚步间隔**：1 Hz 的脚步（间隔
    /// 1000 ms）在 800 ms 下会被切成两个事件，第二个紧接着被冷却期
    /// 挡掉——一次走动只记到一半。
    pub hangover_ms: f64,

    /// 事件结束后不再接受新事件的静默期，毫秒。
    pub cooldown_ms: f64,

    /// 事件结束后仍要继续录进 WAV 的时长，毫秒。
    ///
    /// 与 `cooldown_ms` 是**两件事**，虽然默认值相近：pre-roll 录的是
    /// 「事件之前」，post-roll 录的是「事件之后」。两者都是为了证据
    /// 完整——一段从半空中开始的录音对物业没有说服力。
    ///
    /// 这个值同时是冷却期的**下限来源**：`gate.rs` 里冷却期取
    /// `max(cooldown_ms, post_roll_ms + 1500, response_ms + 500)`，
    /// 后两项是不变量，防止配置把冷却期压到比尾音还短。
    pub post_roll_ms: f64,

    // ---- 门控（回放开启时才生效）----

    /// 耦合校正的额外余量，dB。
    ///
    /// 起始阈值取 `max(start_margin_db, coupling_db + 本值)`。实测到自己
    /// 的回放有多响之后，用实测值而不是理论值来抬高门槛。
    pub coupling_guard_db: f64,

    /// L4 速率看门狗的窗口，毫秒。
    pub rate_window_ms: f64,

    /// 窗口内允许的最大回放次数。超过即自动停回放。
    ///
    /// 12 次/分钟意味着正常使用完全够用，而任何残留的失控循环
    /// 都会在几秒内被掐断。
    pub max_responses_per_window: u32,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            frame_ms: 10.0,

            floor_tau_up_s: 30.0,
            floor_tau_down_s: 300.0,
            floor_warmup_s: 10.0,
            floor_warmup_tau_s: 3.0,

            start_margin_db: 15.0,
            stop_margin_db: 6.0,
            min_rise_db: 15.0,
            rise_window_frames: 5,
            min_lf_ratio_db: Some(6.0),
            min_crest_db: None,

            min_impact_decay_db: 10.0,
            impact_confirm_ms: 500.0,
            impact_confirm_min_frames: 5,

            // 1500 而不是 800：**这个值必须大于你想合并的最慢脚步间隔**。
            // 1 Hz 的脚步声（间隔 1000 ms）在 800 ms 下会被切成两个事件，
            // 而第二个紧接着被 cooldown 挡掉——一次走动只记到一半，
            // 漏掉的那几步在证据里就是「不存在」。
            hangover_ms: 1500.0,
            cooldown_ms: 2000.0,
            post_roll_ms: 3000.0,

            coupling_guard_db: 3.0,
            rate_window_ms: 60_000.0,
            max_responses_per_window: 12,
        }
    }
}

impl DetectorConfig {
    /// 每秒钟的分析帧数。
    pub fn frames_per_second(&self) -> f64 {
        1000.0 / self.frame_ms
    }

    /// 检查参数是否自洽。返回第一条不满足的约束。
    ///
    /// 单独做成方法而不是在 `Default` 里 `assert`，是因为这些值来自用户
    /// 可编辑的配置文件——必须能给出可读的错误，而不是 panic。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.frame_ms <= 0.0 || !self.frame_ms.is_finite() {
            return Err(ConfigError::NonPositive("frame_ms"));
        }
        if self.frame_ms > 100.0 {
            return Err(ConfigError::OutOfRange {
                field: "frame_ms",
                reason: "超过 100 ms 就不是短时分析了，低频时间分辨会丢失",
            });
        }
        if self.rise_window_frames == 0 {
            return Err(ConfigError::NonPositive("rise_window_frames"));
        }
        if self.stop_margin_db >= self.start_margin_db {
            return Err(ConfigError::OutOfRange {
                field: "stop_margin_db",
                reason: "迟滞下阈值必须低于起始阈值，否则没有迟滞",
            });
        }
        if self.floor_tau_up_s <= 0.0 || self.floor_tau_down_s <= 0.0 {
            return Err(ConfigError::NonPositive("floor_tau_*_s"));
        }
        if self.floor_tau_down_s < self.floor_tau_up_s {
            return Err(ConfigError::OutOfRange {
                field: "floor_tau_down_s",
                reason: "底噪必须降得比升得慢，否则安静下来会立刻过于敏感",
            });
        }
        if self.hangover_ms < 0.0 || self.cooldown_ms < 0.0 {
            return Err(ConfigError::OutOfRange {
                field: "hangover_ms/cooldown_ms",
                reason: "时长不能为负",
            });
        }
        if self.max_responses_per_window == 0 {
            return Err(ConfigError::NonPositive("max_responses_per_window"));
        }
        if self.impact_confirm_ms <= 0.0 {
            return Err(ConfigError::NonPositive("impact_confirm_ms"));
        }
        if self.post_roll_ms < 0.0 {
            return Err(ConfigError::NonPositive("post_roll_ms"));
        }
        // NaN 会在所有比较中返回 false，让判据静默失效——一个 NaN 阈值
        // 等于把这道判据关掉，而用户以为自己开着。
        for (field, v) in [
            ("min_lf_ratio_db", self.min_lf_ratio_db),
            ("min_crest_db", self.min_crest_db),
        ] {
            if let Some(v) = v {
                if !v.is_finite() {
                    return Err(ConfigError::OutOfRange {
                        field,
                        reason: "阈值必须是有限数；要关闭请设为 null",
                    });
                }
            }
        }
        Ok(())
    }
}

/// 配置校验失败的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("参数 {0} 必须为正")]
    NonPositive(&'static str),

    #[error("参数 {field} 取值不合理：{reason}")]
    OutOfRange {
        field: &'static str,
        reason: &'static str,
    },
}

#[cfg(test)]
// 「先取默认值、再改其中一个字段」是测试里表达「除了这一项，其余照旧」
// 最清楚的写法；换成结构体字面量会把默认值抄一遍，改默认值时它就过时了。
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        DetectorConfig::default().validate().unwrap();
    }

    #[test]
    fn hysteresis_must_be_ordered() {
        let mut c = DetectorConfig::default();
        c.stop_margin_db = c.start_margin_db;
        assert!(matches!(
            c.validate(),
            Err(ConfigError::OutOfRange {
                field: "stop_margin_db",
                ..
            })
        ));
    }

    #[test]
    fn floor_must_decay_slower_than_it_rises() {
        let mut c = DetectorConfig::default();
        c.floor_tau_down_s = 1.0; // 比上升还快
        assert!(c.validate().is_err());
    }

    #[test]
    fn zero_rise_window_is_rejected() {
        let mut c = DetectorConfig::default();
        c.rise_window_frames = 0;
        assert!(matches!(c.validate(), Err(ConfigError::NonPositive(_))));
    }

    #[test]
    fn frames_per_second_matches_frame_length() {
        assert!((DetectorConfig::default().frames_per_second() - 100.0).abs() < 1e-9);
    }

    /// 旧配置文件缺字段时必须能载入并取默认值，而不是整体失败。
    #[test]
    fn partial_json_fills_in_defaults() {
        let c: DetectorConfig = serde_json::from_str(r#"{"start_margin_db": 20.0}"#).unwrap();
        assert_eq!(c.start_margin_db, 20.0);
        assert_eq!(c.hangover_ms, DetectorConfig::default().hangover_ms);
    }
}
