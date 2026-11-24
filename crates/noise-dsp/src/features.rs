//! 帧特征——交给检测器的窄结构。
//!
//! 这是 DSP 与检测器之间的**唯一**接口。刻意做成扁平的值类型而不是
//! 让检测器直接持有滤波器：检测器可以完全在合成特征流上测试
//! （见 `noise-detect/tests/trigger_scenarios.rs`），不需要音频硬件，
//! 也不需要重新实现一遍 DSP。
//!
//! # 存线性量，不存 dB
//!
//! 两个电平字段存的是**均方值**而非 dB，dB 通过方法现算。理由不是洁癖：
//! 检测器的自适应底噪要在功率域做指数平均（功率可叠加，dB 不可），
//! 存 dB 再转回功率是每帧两次无谓的 `log`/`exp` 往返，而且那个往返
//! 会把「数字静音」压成一个巨大的负 dB 值，进而在上升沿判据里造出
//! 3000 dB 的假增量。
//!
//! # 两条路径在这里分岔
//!
//! * [`Self::trigger_db`] —— 20–200 Hz，**未计权**。检测器只看这个。
//! * `bands_db` —— 八度带，供取证与软标记用。
//!
//! A 计权只进事件记录，**绝不进触发判据**。A 计权在 63 Hz 处衰减 26 dB，
//! 拿它触发等于先把要检测的信号扔掉。

use crate::octave::DEFAULT_CENTERS_HZ;

/// dB 的实用下限。
///
/// 比 `noise_core::units::mean_square_to_db` 用的 `f64::MIN_POSITIVE`
/// （约 −3233 dB）高得多，是刻意的：那个值让「数字静音」与「−200 dBFS
/// 的本底噪声」相差 3000 dB，而这个差值会在上升沿判据里表现为一个
/// 天文数字的「上升」——恰恰出现在最容易出错的地方（合成的脉冲串测试
/// 里，脉冲之间就是精确的数字静音）。
///
/// 代价是「绝对静音」与「极安静」不可区分。在 24 bit 音频里这两者的
/// 差别本来也没有意义。
pub const DB_FLOOR: f64 = -200.0;

/// 均方值 → dB，下限钳到 [`DB_FLOOR`]。
///
/// 返回**有限**值而非 `-inf`：下游的滑动窗口最大值、差值、EWMA 都不该
/// 被迫处理无穷大（`-inf - (-inf) = NaN`）。
#[inline]
// `!(x > 0.0)` 是**有意**写成否定比较的：它同时挡下 0、负数、NaN 和
// `-inf`，而 `x <= 0.0` 放 NaN 过去。clippy 的 `partial_cmp` 建议会
// 把这个区别抹掉，所以在这里明确关掉这条 lint。
#[allow(clippy::neg_cmp_op_on_partial_ord)]
pub fn mean_square_to_db_floored(mean_square: f64, reference: f64) -> f64 {
    if !(mean_square > 0.0) || !reference.is_finite() || reference <= 0.0 {
        return DB_FLOOR;
    }
    let db = 10.0 * (mean_square / (reference * reference)).log10();
    if db.is_finite() {
        db.max(DB_FLOOR)
    } else {
        DB_FLOOR
    }
}

/// dB → 均方值。与 [`mean_square_to_db_floored`] 互逆（在下限之上）。
#[inline]
pub fn db_to_mean_square(db: f64) -> f64 {
    if !db.is_finite() {
        return 0.0;
    }
    10f64.powf(db / 10.0)
}

/// 低频判据用的带下标：63 Hz。
///
/// 脚步声能量在这里达到峰值，也是楼板撞击声评价的中心频率。
pub const LF_BAND_INDEX: usize = 1;

/// 高频参考带的区间（250–500 Hz）。
///
/// 取这一段而非更高频是因为人声和大部分家电噪声落在这里，
/// 用「低频带减去这一段」才能把「低频突出」和「整体变响」分开。
pub const HF_REF_RANGE: std::ops::Range<usize> = 3..5;

/// 单个分析帧的特征。
///
/// `bands_db` 的顺序与 [`DEFAULT_CENTERS_HZ`] 一致。
#[derive(Clone, Debug, PartialEq)]
pub struct FrameFeatures {
    /// 自会话起点的帧序号。用于滑动窗口，与墙上时钟解耦。
    pub frame_index: u64,
    /// 自会话起点的毫秒数。
    pub t_ms: f64,
    /// 触发带（20–200 Hz）的均方值，未计权。
    pub trigger_mean_square: f64,
    /// 宽带均方值，未计权。只作显示与证据，不参与触发。
    pub broadband_mean_square: f64,
    /// 触发带内的帧内峰值因数，dB。
    ///
    /// **默认不参与判据**——实测证明 10 ms 帧下它无法区分冲击与稳态
    /// 噪声，原因见 `docs/detector-tuning.md` 第一节。保留它供事后审阅
    /// 和实测推翻预测。
    pub crest_db: f64,
    /// 八度带电平，dB，顺序同 [`DEFAULT_CENTERS_HZ`]。
    pub bands_db: Vec<f64>,
    /// 频谱平坦度（线性功率的几何均值/算术均值，0–1）。
    ///
    /// **只作软标记，不做硬拒绝**。平坦度区分的是「宽带噪声 vs 纯音」，
    /// 而空调和车流本身就是低频主导的连续噪声——它们的平坦度与脚步同样低。
    pub flatness: f64,
    /// 转 dB 时使用的参考（`noise_core::P0` 得 dB SPL，`1.0` 得 dBFS）。
    pub reference: f64,
}

impl FrameFeatures {
    /// 触发带电平，dB。**检测器唯一该看的电平。**
    pub fn trigger_db(&self) -> f64 {
        mean_square_to_db_floored(self.trigger_mean_square, self.reference)
    }

    /// 宽带电平，dB。仅用于显示与证据。
    pub fn broadband_db(&self) -> f64 {
        mean_square_to_db_floored(self.broadband_mean_square, self.reference)
    }

    /// 63 Hz 带相对 250–500 Hz 段的突出程度，dB。正值表示低频突出。
    pub fn lf_ratio_db(&self) -> f64 {
        let lf = self.bands_db.get(LF_BAND_INDEX).copied().unwrap_or(0.0);
        let hf = self
            .bands_db
            .get(HF_REF_RANGE)
            .map(|s| s.iter().cloned().fold(f64::NEG_INFINITY, f64::max))
            .unwrap_or(0.0);
        lf - hf
    }

    /// 某个中心频率的带电平。找不到时返回 `None`。
    pub fn band_at_hz(&self, fc: f64) -> Option<f64> {
        DEFAULT_CENTERS_HZ
            .iter()
            .position(|c| (c - fc).abs() < 1e-9)
            .and_then(|i| self.bands_db.get(i).copied())
    }
}

/// 频谱平坦度：线性功率的几何均值 / 算术均值。
///
/// 输入是 dB 值。全带等能量时为 1.0，能量集中到单一频带时趋近 0。
///
/// 用对数域求几何均值（`exp(mean(ln x))`）而不是连乘——dB 值要先转回
/// 线性功率，而 6 个 `10^(x/10)` 连乘在 x 较大时会上溢。
pub fn spectral_flatness_db(bands_db: &[f64]) -> f64 {
    let powers: Vec<f64> = bands_db.iter().map(|db| db_to_mean_square(*db)).collect();
    let n = powers.len();
    if n == 0 {
        return 0.0;
    }
    let sum: f64 = powers.iter().sum();
    if sum <= 0.0 || !sum.is_finite() {
        return 0.0;
    }
    let log_sum: f64 = powers
        .iter()
        .map(|p| if *p > 0.0 { p.ln() } else { f64::NEG_INFINITY })
        .sum();
    if !log_sum.is_finite() {
        return 0.0;
    }
    let geo_mean = (log_sum / n as f64).exp();
    let arith_mean = sum / n as f64;
    (geo_mean / arith_mean).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(trigger_ms: f64, bands: Vec<f64>) -> FrameFeatures {
        let flatness = spectral_flatness_db(&bands);
        FrameFeatures {
            frame_index: 0,
            t_ms: 0.0,
            trigger_mean_square: trigger_ms,
            broadband_mean_square: trigger_ms,
            crest_db: 0.0,
            bands_db: bands,
            flatness,
            reference: 1.0,
        }
    }

    #[test]
    fn trigger_db_converts_from_mean_square() {
        let f = features(0.5, vec![0.0; 6]);
        // 全刻度正弦的均方值 0.5 → -3.01 dBFS
        assert!((f.trigger_db() - (-3.010_299_956_639_812)).abs() < 1e-9);
    }

    #[test]
    fn digital_silence_hits_the_floor_not_negative_infinity() {
        let f = features(0.0, vec![0.0; 6]);
        assert_eq!(f.trigger_db(), DB_FLOOR);
        assert!(f.trigger_db().is_finite());
    }

    #[test]
    fn db_round_trip_is_lossless_above_the_floor() {
        for db in [-100.0, -60.0, -20.5, -3.01, 0.0] {
            let ms = db_to_mean_square(db);
            assert!((mean_square_to_db_floored(ms, 1.0) - db).abs() < 1e-9, "db={db}");
        }
    }

    /// 下限的作用：数字静音与极安静之间的「上升」不该是天文数字。
    #[test]
    fn floor_keeps_synthetic_rises_sane() {
        let silent = features(0.0, vec![0.0; 6]).trigger_db();
        let quiet = features(db_to_mean_square(-90.0), vec![0.0; 6]).trigger_db();
        assert!(
            quiet - silent < 120.0,
            "静音到 -90 dBFS 的增量应是有界的，实得 {:.1} dB（下限 {DB_FLOOR}）",
            quiet - silent
        );
    }

    #[test]
    fn flatness_is_one_for_equal_bands() {
        let f = spectral_flatness_db(&[60.0, 60.0, 60.0, 60.0, 60.0, 60.0]);
        assert!((f - 1.0).abs() < 1e-9, "实得 {f}");
    }

    #[test]
    fn flatness_drops_when_energy_is_concentrated() {
        let flat_ish = spectral_flatness_db(&[60.0, 62.0, 58.0, 61.0, 59.0, 60.0]);
        let peaked = spectral_flatness_db(&[90.0, 60.0, 55.0, 50.0, 48.0, 45.0]);
        assert!(flat_ish > 0.9, "近平坦应接近 1，实得 {flat_ish}");
        assert!(peaked < 0.1, "单带突出应接近 0，实得 {peaked}");
    }

    #[test]
    fn flatness_handles_degenerate_input() {
        assert_eq!(spectral_flatness_db(&[]), 0.0);
        assert!(spectral_flatness_db(&[DB_FLOOR; 6]).is_finite());
        assert!(spectral_flatness_db(&[f64::NAN; 6]).is_finite());
    }

    #[test]
    fn lf_ratio_is_positive_when_low_band_dominates() {
        let f = features(0.1, vec![65.0, 70.0, 60.0, 50.0, 50.0, 48.0]);
        assert!((f.lf_ratio_db() - 20.0).abs() < 1e-9, "实得 {}", f.lf_ratio_db());
    }

    #[test]
    fn lf_ratio_is_negative_for_high_frequency_dominant_audio() {
        let f = features(0.1, vec![45.0, 48.0, 55.0, 70.0, 68.0, 66.0]);
        assert!(f.lf_ratio_db() < 0.0, "实得 {}", f.lf_ratio_db());
    }

    #[test]
    fn band_at_hz_finds_known_centers() {
        let f = features(0.1, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(f.band_at_hz(63.0), Some(2.0));
        assert_eq!(f.band_at_hz(1000.0), Some(6.0));
        assert_eq!(f.band_at_hz(37.0), None);
    }
}
