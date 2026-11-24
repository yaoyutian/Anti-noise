//! 事件分类的领域词汇。
//!
//! 放在 `noise-core` 而不是 `noise-detect` 或 `noise-store`：检测器要产出它，
//! 存储层要持久化它，UI 要显示它。放进任何一侧都会制造反向依赖。

use serde::{Deserialize, Serialize};

/// 一次噪声事件的类别。
///
/// 分类**在事件结束时**才定（见 `noise_detect::sm`），但依据不是「结束时
/// 电平有没有降下去」——那样一个走着的人配一台开着的空调就会被判成持续型，
/// 丢掉「有人在走」这个结论。依据是**事件期间有没有确认过冲击**：每一次
/// 上升沿都只在电平随后衰减了 `min_impact_decay_db` 之后才被计为冲击。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// 冲击型：电平骤起后**衰减**。脚步声、拖家具、敲击。
    ///
    /// 这是用户真正要取证的那一类。
    Impact,

    /// 持续型：电平骤起后**维持不降**。空调启动、洗衣机、水泵。
    ///
    /// 单列一类而不是丢弃，是因为它同样是扰民事实，值得留一条记录；
    /// 但它不是脚步，混进冲击事件里会淹没真正要找的东西。
    Sustained,

    /// 用户手动标记。
    Manual,

    /// 校准信号（声校准器的 1 kHz 音）。
    ///
    /// 必须与真实噪声分开，否则校准过程会污染证据链。
    Calibration,
}

impl EventKind {
    /// 是否计入「邻居噪音」统计。
    ///
    /// 校准和手动标记都不计——它们不是环境事件。
    pub fn counts_as_neighbor_noise(self) -> bool {
        matches!(self, Self::Impact | Self::Sustained)
    }

    /// 是否值得触发反击回放。
    ///
    /// 只有冲击型触发。持续型回放一段 400 ms 的闷响毫无意义，
    /// 而且会把回放次数配额（L4 看门狗，12 次/分钟）浪费掉。
    pub fn warrants_response(self) -> bool {
        matches!(self, Self::Impact)
    }
}

/// 事件的可选标记位。**不参与任何判据**，只随事件存档供事后审阅。
///
/// 刻意用标志位而不是枚举：这些性质可以同时成立，而且将来想加新的
/// 软标记时不必改动已有的持久化格式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EventFlags {
    /// 频谱平坦度低（能量集中在少数频带）。
    ///
    /// 注意这**不是**「冲击」的指示——空调和车流本身就是低频集中的。
    /// 它只说明「这条事件的主要能量来源是窄带的」。
    pub narrowband: bool,

    /// 低频突出（63 Hz 带显著高于 250–500 Hz 段）。
    pub low_frequency_dominant: bool,

    /// 录音期间包含本机自己的反击回放。取证时必须标注，否则对方可以
    /// 反过来质疑「这声音是你自己放的」。
    pub self_generated: bool,

    /// 采样率或滤波器设计发生了降级（见 `noise_dsp` 的 `degraded` 标记）。
    pub degraded_dsp: bool,

    /// 事件期间音频回调发生过丢帧。
    pub dropped_audio: bool,
}

impl EventFlags {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_impact_warrants_a_response() {
        assert!(EventKind::Impact.warrants_response());
        // 持续型回放没有意义，而且会浪费 L4 速率配额
        assert!(!EventKind::Sustained.warrants_response());
        assert!(!EventKind::Manual.warrants_response());
        assert!(!EventKind::Calibration.warrants_response());
    }

    #[test]
    fn calibration_does_not_count_as_noise() {
        assert!(!EventKind::Calibration.counts_as_neighbor_noise());
        assert!(!EventKind::Manual.counts_as_neighbor_noise());
        assert!(EventKind::Impact.counts_as_neighbor_noise());
        assert!(EventKind::Sustained.counts_as_neighbor_noise());
    }

    #[test]
    fn event_kind_serializes_as_snake_case() {
        let s = serde_json::to_string(&EventKind::Impact).unwrap();
        assert_eq!(s, "\"impact\"");
        let s = serde_json::to_string(&EventKind::Sustained).unwrap();
        assert_eq!(s, "\"sustained\"");
    }

    #[test]
    fn default_flags_are_empty() {
        assert!(EventFlags::default().is_empty());
        let f = EventFlags {
            self_generated: true,
            ..Default::default()
        };
        assert!(!f.is_empty());
    }
}
