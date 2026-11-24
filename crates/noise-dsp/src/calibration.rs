//! 麦克风校准。
//!
//! # 为什么需要它
//!
//! `cpal` 只给出归一化浮点数，**驱动层完全没有绝对声压参考**。没有校准，
//! 「邻居达到 68 dB(A)」这种陈述是不可辩护的，只能报告相对电平（dBFS）。
//!
//! # 校准流程
//!
//! 声校准器（IEC 60942）向麦克风施加一个已知声压，标准值是
//! **94 dB SPL（1 Pa RMS）@ 1 kHz**。测得此时的数字电平 `r_cal`（dBFS），
//! 即得偏移：
//!
//! ```text
//! offset = L_cal - dBFS(r_cal)
//! L_p    = dBFS + offset
//! ```
//!
//! ## 两个容易忽略的细节
//!
//! 1. **1 kHz 处 A 计权恰好是 0.00 dB**——这正是标准选 1 kHz 的原因。
//!    所以用 1 kHz 校准音时，在 A 计权通路上读数是免修正的。但若使用
//!    250 Hz 校准器（A 计权在该处是 −0.3 dB），必须在 C 或 Z 计权下校准，
//!    否则要手工补回这个差值。
//!
//! 2. **自由场修正**：校准器提供的是**压力场**信号，而 IEC 61672 的容差
//!    是按**自由场**定义的，两者在 1 kHz 上差 0.1–0.2 dB。所以许多厂商
//!    把目标读数定为 93.8–93.9 dB 而不是 94.0。本项目把这个决定权交给
//!    用户——他填什么参考值，就用什么参考值。

use noise_core::units::{Dbfs, DbSpl};
use serde::{Deserialize, Serialize};

/// 声校准器的标准输出：94 dB SPL @ 1 kHz。
pub const DEFAULT_REFERENCE_DB_SPL: f64 = 94.0;

/// 建立校准时使用的默认参考值。
fn default_reference_db_spl() -> f64 {
    DEFAULT_REFERENCE_DB_SPL
}

/// 校准状态。
///
/// 存**偏移**而不是「94 dB SPL 时读到的 dBFS」，是因为自由场修正让参考值
/// 本身成为一个变量（93.9 与 94.0 都合理），把参考值烘进偏移里之后
/// 换算路径上就只剩一个加法，没有第二处可以搞错。
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    /// `L_p = dBFS + offset_db` 里的这个 offset。
    ///
    /// `None` 表示**未校准**——此时 UI 必须只显示 dBFS，绝不能声称绝对声压级。
    #[serde(default)]
    pub offset_db: Option<f64>,

    /// 建立校准时声校准器的标称输出，**仅作审计留痕**。
    ///
    /// 换算只用 [`Self::offset_db`]；这个字段存在的意义是事后能说清
    /// 「当初是按 94.0 还是按自由场修正的 93.9 校的」，而这件事
    /// 无法从偏移值反推出来。
    #[serde(default = "default_reference_db_spl")]
    pub reference_db_spl: f64,
}

impl Calibration {
    /// 未校准。绝对声压级不可用。
    pub fn uncalibrated() -> Self {
        Self {
            offset_db: None,
            reference_db_spl: DEFAULT_REFERENCE_DB_SPL,
        }
    }

    /// 由一次实测建立校准。
    ///
    /// * `reference_db_spl` —— 校准器的标称输出（通常 94.0，或按自由场修正取 93.9）
    /// * `measured_dbfs` —— 此时软件测得的电平
    pub fn from_reference(reference_db_spl: f64, measured_dbfs: f64) -> Self {
        debug_assert!(
            measured_dbfs <= 0.0 + 1e-9,
            "dBFS 读数不应为正：{measured_dbfs}"
        );
        Self {
            offset_db: Some(reference_db_spl - measured_dbfs),
            reference_db_spl,
        }
    }

    pub fn is_calibrated(&self) -> bool {
        self.offset_db.is_some()
    }

    /// `dBFS + offset = dB SPL` 里的这个 offset。
    pub fn offset_db(&self) -> Option<f64> {
        self.offset_db
    }

    /// dBFS → dB SPL。未校准时返回 `None`。
    ///
    /// 返回 `Option` 而非一个「尽力而为」的 f64 是刻意的：调用方必须
    /// 显式处理未校准的情况，而不能不小心把相对电平当绝对声压级报出去。
    pub fn dbfs_to_dbspl(&self, dbfs: Dbfs) -> Option<DbSpl> {
        self.offset_db().map(|o| DbSpl(dbfs.0 + o))
    }

    /// dB SPL → dBFS。未校准时返回 `None`。
    pub fn dbspl_to_dbfs(&self, dbspl: DbSpl) -> Option<Dbfs> {
        self.offset_db().map(|o| Dbfs(dbspl.0 - o))
    }

    /// 把均方值直接转成 dB SPL，未校准时返回 `None`。
    pub fn mean_square_to_dbspl(&self, mean_square: f64) -> Option<DbSpl> {
        self.dbfs_to_dbspl(noise_core::units::mean_square_to_dbfs(mean_square))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncalibrated_reports_none_for_absolute_levels() {
        let c = Calibration::uncalibrated();
        assert!(!c.is_calibrated());
        assert_eq!(c.offset_db(), None);
        assert!(c.dbfs_to_dbspl(Dbfs(-30.0)).is_none());
        assert!(c.mean_square_to_dbspl(0.001).is_none());
    }

    #[test]
    fn calibrator_reading_produces_94_dbspl() {
        // 校准器输出 94 dB SPL，软件读到 -30 dBFS ⇒ offset = 124
        let c = Calibration::from_reference(94.0, -30.0);
        assert_eq!(c.offset_db(), Some(124.0));
        assert_eq!(c.dbfs_to_dbspl(Dbfs(-30.0)), Some(DbSpl(94.0)));
    }

    #[test]
    fn round_trip_is_identity() {
        let c = Calibration::from_reference(94.0, -27.5);
        for dbfs in [-60.0, -30.0, -12.0, 0.0] {
            let spl = c.dbfs_to_dbspl(Dbfs(dbfs)).unwrap();
            let back = c.dbspl_to_dbfs(spl).unwrap();
            assert!((back.0 - dbfs).abs() < 1e-12);
        }
    }

    #[test]
    fn full_scale_sine_is_consistent_with_mean_square_path() {
        // 全刻度正弦：均方 = 0.5，即 -3.01 dBFS
        let c = Calibration::from_reference(94.0, -30.0);
        let via_ms = c.mean_square_to_dbspl(0.5).unwrap();
        let via_dbfs = c.dbfs_to_dbspl(Dbfs(-3.010_299_956_639_812)).unwrap();
        assert!((via_ms.0 - via_dbfs.0).abs() < 1e-9, "{via_ms:?} vs {via_dbfs:?}");
    }

    /// 自由场修正：厂商常把目标定为 93.9 而非 94.0。
    #[test]
    fn free_field_correction_is_honoured() {
        let c = Calibration::from_reference(93.9, -30.0);
        assert!((c.offset_db().unwrap() - 123.9).abs() < 1e-12);
    }
}
