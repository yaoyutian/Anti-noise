//! 迟滞阈值：从底噪和余量推导起始/停止门限，并施加实测耦合校正。
//!
//! # 为什么需要迟滞
//!
//! 单一阈值会让电平在阈值附近抖动时反复触发/结束。迟滞用两个门限：
//! 向上穿过 `start_db` 才起始事件，向下穿过 `stop_db` 才结束。
//! 两者之差就是死区。
//!
//! # 耦合校正（L3）
//!
//! 回放门控的 L1/L2 挡住的是**回放当下**的重触发。L3 挡的是一种更
//! 隐蔽的情况：我们把音响开得比预期响，房间衰减比预期慢，于是冷却期
//! 结束时房间里仍有可观能量——或者更糟，麦克风离音响太近，一段
//! 混响尾巴刚好越过阈值。
//!
//! L3 的输入是 [`HalfDuplexGate::coupling_excess_db`]——回放期间实测
//! 「自己的声音在麦克风处高出底噪多少 dB」。有了这个数，阈值可以从
//! 理论值改成实测值：
//!
//! ```text
//! 有效起始余量 = max(start_margin_db, 实测耦合超出 + coupling_guard_db)
//! ```
//!
//! 取 `max` 而不是直接替换是有意的：耦合校正只能让系统**更保守**，
//! 永远不能让它比用户设定的更灵敏。否则一次安静的校准（比如用户
//! 校准时音响没开）会把阈值压到地板以下，系统随即失控。
//!
//! [`HalfDuplexGate::coupling_excess_db`]: crate::gate::HalfDuplexGate::coupling_excess_db

use crate::config::DetectorConfig;

/// 一帧的判定门限，由当前底噪推导。
///
/// 底噪每帧都在动，所以这个结构**每帧重算**——它是廉价的值类型，
/// 不是需要缓存的状态。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    /// 起始门限，绝对 dB（已含底噪）。
    pub start_db: f64,
    /// 停止门限，绝对 dB。恒低于 `start_db`。
    pub stop_db: f64,
    /// 实际生效的起始余量，dB。等于 `start_db - floor_db`。
    pub start_margin_db: f64,
    /// 本次推导是否用上了实测耦合校正。
    pub coupling_corrected: bool,
    /// 推导时使用的底噪，dB。保留下来供 UI 显示和事后对账。
    pub floor_db: f64,
}

/// 从底噪和配置推导门限。
///
/// * `floor_db` —— 当前底噪，绝对 dB
/// * `coupling_excess_db` —— 实测耦合超出量；`None` 表示还没测到
///   （回放关闭、或还没回放过）——此时退化为纯理论余量
pub fn derive(
    floor_db: f64,
    cfg: &DetectorConfig,
    coupling_excess_db: Option<f64>,
) -> Thresholds {
    let mut start_margin = cfg.start_margin_db;
    let mut coupling_corrected = false;

    if let Some(excess) = coupling_excess_db {
        if excess.is_finite() {
            let corrected = excess + cfg.coupling_guard_db;
            if corrected > start_margin {
                start_margin = corrected;
                coupling_corrected = true;
            }
        }
    }

    // 底噪本身可能落在 DB_FLOOR 上（会话刚启动、或麦克风静音）。
    // 此时 start_db 会是一个极低的数——这不是问题，`rise` 判据和
    // `min_lf_ratio_db` 会挡住绝大多数误报，而底噪几秒内就会初始化。
    let start_db = floor_db + start_margin;

    // 停止门限**不**施加耦合校正：它只在事件进行中被查询，而事件进行中
    // 不可能有回放（回放会把事件结束掉，见 `sm::Detector`）。对它加压
    // 只会让事件该结束时结束不了——那是把「更保守」用错了方向。
    //
    // 但停止门限必须**严格**低于起始门限。`validate()` 已经保证了配置
    // 层面的 `stop_margin_db < start_margin_db`，可耦合校正只抬高起始边。
    // 校正生效时两者差距只会更大，所以这里不需要再夹一次。
    let stop_db = floor_db + cfg.stop_margin_db;

    Thresholds {
        start_db,
        stop_db,
        start_margin_db: start_margin,
        coupling_corrected,
        floor_db,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOOR: f64 = -60.0;

    #[test]
    fn without_coupling_it_is_the_plain_margin() {
        let cfg = DetectorConfig::default();
        let t = derive(FLOOR, &cfg, None);
        assert_eq!(t.start_db, FLOOR + cfg.start_margin_db);
        assert_eq!(t.stop_db, FLOOR + cfg.stop_margin_db);
        assert!(!t.coupling_corrected);
    }

    #[test]
    fn known_coupling_raises_the_start_threshold() {
        let cfg = DetectorConfig::default();
        // 回放在麦克风处高出底噪 40 dB，守卫余量 3 dB → 有效余量 43
        let t = derive(FLOOR, &cfg, Some(40.0));
        assert_eq!(t.start_margin_db, 43.0);
        assert_eq!(t.start_db, FLOOR + 43.0);
        assert!(t.coupling_corrected);
    }

    /// 耦合校正**只能更保守**，绝不能把阈值压得更灵敏。
    #[test]
    fn weak_coupling_cannot_lower_the_margin() {
        let cfg = DetectorConfig::default(); // start_margin = 15
        // 实测耦合只有 2 dB 超出，加 3 dB 守卫 = 5，远低于 15
        let t = derive(FLOOR, &cfg, Some(2.0));
        assert_eq!(t.start_margin_db, cfg.start_margin_db);
        assert!(!t.coupling_corrected, "未生效时不该声称已校正");
    }

    /// 极端情况：回放比底噪还轻（麦在另一个房间），耦合超出为 0。
    #[test]
    fn zero_coupling_is_harmless() {
        let cfg = DetectorConfig::default();
        let t = derive(FLOOR, &cfg, Some(0.0));
        assert_eq!(t.start_margin_db, cfg.start_margin_db);
        assert!(!t.coupling_corrected);
    }

    #[test]
    fn hysteresis_gap_is_preserved() {
        let cfg = DetectorConfig::default();
        for coupling in [None, Some(0.0), Some(5.0), Some(40.0), Some(120.0)] {
            let t = derive(FLOOR, &cfg, coupling);
            assert!(
                t.start_db > t.stop_db,
                "起始门限必须严格高于停止门限（coupling={coupling:?}）：{} vs {}",
                t.start_db,
                t.stop_db
            );
        }
    }

    #[test]
    fn non_finite_coupling_is_ignored() {
        let cfg = DetectorConfig::default();
        for bad in [f64::NAN, f64::INFINITY] {
            let t = derive(FLOOR, &cfg, Some(bad));
            assert_eq!(t.start_margin_db, cfg.start_margin_db);
            assert!(!t.coupling_corrected);
        }
    }

    /// 阈值随底噪平移——这是「自适应」的全部含义。
    #[test]
    fn thresholds_track_the_floor() {
        let cfg = DetectorConfig::default();
        let quiet = derive(-70.0, &cfg, None);
        let loud = derive(-40.0, &cfg, None);
        assert_eq!(loud.start_db - quiet.start_db, 30.0);
        assert_eq!(loud.stop_db - quiet.stop_db, 30.0);
    }

    #[test]
    fn effective_margin_is_reported_consistently() {
        let cfg = DetectorConfig::default();
        let t = derive(FLOOR, &cfg, Some(25.0));
        assert!((t.start_db - (t.floor_db + t.start_margin_db)).abs() < 1e-12);
    }
}
