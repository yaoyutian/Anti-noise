//! 声级指标。
//!
//! # 时间计权作用在**平方后**的声压上
//!
//! 这是最容易搞错的一点。正确形式是
//!
//! ```text
//! y[n] = α·x[n]² + (1-α)·y[n-1],   α = 1 - exp(-1/(fs·τ))
//! ```
//!
//! 而不是作用在幅度上。判据：Slow 计权（τ = 1 s）的衰减率规范值是
//! **4.34 dB/s**，而 `10·log₁₀(e)/τ = 4.343`——只有 α 作用在平方上才
//! 能得到这个数。作用在幅度上会让衰减常数差一倍。
//!
//! # Leq 不加时间计权
//!
//! Leq 是纯粹的能量平均。它和 Fast/Slow 是两条独立的路径，把两者混在
//! 一起是常见错误。

use noise_core::units::{mean_square_to_db, mean_square_to_dbfs, Dbfs};
use noise_core::P0;

/// 线性均方值。
#[inline]
pub fn mean_square(x: &[f64]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    x.iter().map(|s| s * s).sum::<f64>() / x.len() as f64
}

/// 绝对值峰值。
#[inline]
pub fn peak_abs(x: &[f64]) -> f64 {
    x.iter().fold(0.0f64, |m, s| m.max(s.abs()))
}

/// 均方值 + 样本数 → 均值（用于分块累加的场景）。
#[inline]
pub fn mean_square_from_sum(sum_sq: f64, n: usize) -> f64 {
    if n == 0 {
        0.0
    } else {
        sum_sq / n as f64
    }
}

/// 峰值因数（波峰因数），dB。
///
/// 这是区分「冲击」和「连续」的核心特征：10 ms 帧内，脚步之类的冲击
/// 读数为 10–20 dB，而风扇/车流之类的稳态低频噪声只有 3–6 dB。
#[inline]
pub fn crest_factor_db(peak: f64, rms: f64) -> f64 {
    const FLOOR: f64 = 1e-20;
    if rms <= FLOOR {
        return 0.0;
    }
    20.0 * (peak.max(FLOOR) / rms).log10()
}

/// 均方值 → dBFS。
#[inline]
pub fn dbfs_from_mean_square(ms: f64) -> Dbfs {
    mean_square_to_dbfs(ms)
}

/// 均方值 → dB SPL（未校准时无绝对意义，见 [`crate::calibration`]）。
#[inline]
pub fn dbspl_from_mean_square(ms: f64) -> f64 {
    mean_square_to_db(ms, P0)
}

/// IEC 61672-1 时间计权。
///
/// 刻意**不提供** Impulse：它的 attack/decay 时间常数不对称（35 ms / 1500 ms），
/// 因而不是一个线性滤波器、不可叠加，IEC 61672-1 已把它降级到资料性附录 C
/// 并明确指出它不适合评价脉冲声。这里没有它能派上用场的场景。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeWeighting {
    /// τ = 125 ms，衰减率约 34.7 dB/s。
    Fast,
    /// τ = 1000 ms，衰减率约 4.34 dB/s。
    Slow,
}

impl TimeWeighting {
    /// 时间常数，单位秒。
    pub fn tau_s(self) -> f64 {
        match self {
            Self::Fast => 0.125,
            Self::Slow => 1.0,
        }
    }

    /// 递归系数 `α = 1 - exp(-1/(fs·τ))`。
    ///
    /// 从公式现算，**不要硬编码**——硬编码的字面量无法跟随采样率变化，
    /// 而这个项目里采样率是运行时才知道的。
    pub fn alpha(self, fs: f64) -> f64 {
        1.0 - (-1.0 / (fs * self.tau_s())).exp()
    }
}

/// 作用在平方声压上的一阶指数平均器。
///
/// 按块更新而非逐样本，因此与音频回调的缓冲大小无关。分块形式：
///
/// ```text
/// y_out = (1-β)·y_in + β·mean_sq,   β = 1 - (1-α)^n
/// ```
///
/// 当块内功率近似恒定（10 ms 帧对 125 ms 时间常数而言是成立的），
/// 这与逐样本递推在数学上等价。
#[derive(Clone, Debug)]
pub struct SquaredEwma {
    y: f64,
    alpha: f64,
}

impl SquaredEwma {
    pub fn new(tw: TimeWeighting, fs: f64) -> Self {
        Self {
            y: 0.0,
            alpha: tw.alpha(fs),
        }
    }

    /// 喂入一个块的**能量和**（Σx²，未除以 n）与样本数，返回更新后的均方值。
    pub fn update(&mut self, block_sq_sum: f64, n: usize) -> f64 {
        if n == 0 {
            return self.y;
        }
        let mean = block_sq_sum / n as f64;
        // powf 而非 powi：n 可能很大，用 i32 会溢出。
        let beta = 1.0 - (1.0 - self.alpha).powf(n as f64);
        self.y = (1.0 - beta) * self.y + beta * mean;
        self.y
    }

    pub fn value(&self) -> f64 {
        self.y
    }

    /// 当前值的 dBFS。
    pub fn dbfs(&self) -> Dbfs {
        mean_square_to_dbfs(self.y)
    }

    pub fn reset(&mut self) {
        self.y = 0.0;
    }
}

/// Leq 累加器：纯能量平均，**不加时间计权**。
///
/// 同时维护整段运行值和可重置的区间值——持久监测两者都需要。
#[derive(Clone, Debug, Default)]
pub struct LeqAccumulator {
    sum_sq: f64,
    n: u64,
}

impl LeqAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一个块的能量和与样本数。
    pub fn push(&mut self, block_sq_sum: f64, n: usize) {
        if block_sq_sum.is_finite() && block_sq_sum >= 0.0 {
            self.sum_sq += block_sq_sum;
        }
        self.n += n as u64;
    }

    /// 运行 Leq，dB SPL。未校准时只有相对意义。
    pub fn leq_dbspl(&self) -> f64 {
        if self.n == 0 {
            return f64::NEG_INFINITY;
        }
        mean_square_to_db(self.sum_sq / self.n as f64, P0)
    }

    /// 运行 Leq，dBFS。
    pub fn leq_dbfs(&self) -> Dbfs {
        if self.n == 0 {
            return Dbfs(f64::NEG_INFINITY);
        }
        mean_square_to_dbfs(self.sum_sq / self.n as f64)
    }

    /// 已累计的样本数——用来判断 Leq 是否已有意义（分母太小时不可信）。
    pub fn samples(&self) -> u64 {
        self.n
    }

    pub fn reset(&mut self) {
        self.sum_sq = 0.0;
        self.n = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen;

    #[test]
    fn mean_square_of_full_scale_sine_is_half() {
        let x = testgen::sine(1000.0, 48_000.0, 48_000, 1.0);
        let ms = mean_square(&x);
        assert!((ms - 0.5).abs() < 1e-6, "实得 {ms}");
    }

    #[test]
    fn doubling_amplitude_is_plus_six_db() {
        let a = testgen::sine(1000.0, 48_000.0, 4800, 0.25);
        let b = testgen::sine(1000.0, 48_000.0, 4800, 0.5);
        let diff = dbfs_from_mean_square(mean_square(&b)).0 - dbfs_from_mean_square(mean_square(&a)).0;
        assert!((diff - 6.020_599_913_279_624).abs() < 1e-6, "实得 {diff}");
    }

    /// 从公式重算 α，而不是复述字面量。
    #[test]
    fn fast_alpha_matches_formula() {
        let fs: f64 = 48_000.0;
        let expected = 1.0 - (-1.0 / (fs * 0.125)).exp();
        assert!((TimeWeighting::Fast.alpha(fs) - expected).abs() < 1e-15);
    }

    #[test]
    fn slow_alpha_matches_formula() {
        let fs: f64 = 48_000.0;
        let expected = 1.0 - (-1.0 / (fs * 1.0)).exp();
        assert!((TimeWeighting::Slow.alpha(fs) - expected).abs() < 1e-15);
    }

    /// Slow 计权的规范衰减率是 4.34 dB/s。这条测试锁定「时间计权作用在
    /// 平方上」这个约定——作用在幅度上会得到约 8.7 dB/s。
    #[test]
    fn slow_weighting_decays_at_iec_specified_rate() {
        let decay_db_per_s = 10.0 * std::f64::consts::E.log10() / TimeWeighting::Slow.tau_s();
        assert!(
            (decay_db_per_s - 4.34).abs() < 0.01,
            "Slow 衰减率应约 4.34 dB/s，实得 {decay_db_per_s:.3}"
        );
    }

    #[test]
    fn ewma_converges_to_steady_state_level() {
        let fs = 48_000.0;
        let x = testgen::sine(1000.0, fs, 48_000, 1.0);
        let mut w = SquaredEwma::new(TimeWeighting::Fast, fs);
        // 喂 1 秒，远超 5τ = 625 ms
        let sum: f64 = x.iter().map(|s| s * s).sum();
        w.update(sum, x.len());
        // 稳态应接近 0.5（全刻度正弦的均方值）
        assert!((w.value() - 0.5).abs() < 0.01, "实得 {}", w.value());
    }

    #[test]
    fn ewma_block_size_does_not_change_result() {
        let fs = 48_000.0;
        let x = testgen::sine(1000.0, fs, 48_000, 0.7);

        // 整块喂
        let mut whole = SquaredEwma::new(TimeWeighting::Fast, fs);
        let s: f64 = x.iter().map(|v| v * v).sum();
        whole.update(s, x.len());

        // 按 480 样本分块喂
        let mut chunked = SquaredEwma::new(TimeWeighting::Fast, fs);
        for c in x.chunks(480) {
            let s: f64 = c.iter().map(|v| v * v).sum();
            chunked.update(s, c.len());
        }

        let diff = (whole.value() - chunked.value()).abs();
        assert!(diff < 1e-9, "分块大小不应影响结果，差异 {diff}");
    }

    #[test]
    fn leq_of_sine_matches_analytic_value() {
        let fs = 48_000.0;
        let amp = 0.5;
        let x = testgen::sine(1000.0, fs, fs as usize, amp);
        let mut acc = LeqAccumulator::new();
        let s: f64 = x.iter().map(|v| v * v).sum();
        acc.push(s, x.len());

        // 解析值：均方 = A²/2
        let expected = mean_square_to_db(amp * amp / 2.0, P0);
        assert!(
            (acc.leq_dbspl() - expected).abs() < 0.05,
            "期望 {expected:.4}，实得 {:.4}",
            acc.leq_dbspl()
        );
    }

    #[test]
    fn leq_is_energy_average_not_arithmetic_mean_of_db() {
        let mut acc = LeqAccumulator::new();
        // 两段等长：一段静音，一段 -20 dBFS
        acc.push(0.0, 1000);
        acc.push(0.01 * 1000.0, 1000);
        // 能量平均 = (0 + 0.01)/2 = 0.005 → -23.01 dBFS
        // 若错误地取了 dB 的算术平均会得到 -inf 或别的值
        let expected = 10.0 * (0.005f64).log10();
        assert!((acc.leq_dbfs().0 - expected).abs() < 1e-9, "{:?}", acc.leq_dbfs());
    }

    #[test]
    fn crest_factor_separates_impulse_from_steady_noise() {
        // 稳态正弦：峰值/有效值 = √2 → 3.01 dB
        let s = testgen::sine(100.0, 48_000.0, 4800, 1.0);
        let c = crest_factor_db(peak_abs(&s), mean_square(&s).sqrt());
        assert!((c - 3.010_299_956_639_812).abs() < 1e-3, "实得 {c}");

        // 单个脉冲：峰均比很高
        let mut impulse = vec![0.0; 4800];
        impulse[2400] = 1.0;
        let c2 = crest_factor_db(peak_abs(&impulse), mean_square(&impulse).sqrt());
        assert!(c2 > 30.0, "脉冲的峰值因数应很高，实得 {c2}");
    }

    #[test]
    fn crest_factor_handles_silence_without_nan() {
        let c = crest_factor_db(0.0, 0.0);
        assert!(c.is_finite(), "静音输入产生了 {c}");
    }

    #[test]
    fn empty_accumulator_reports_negative_infinity() {
        let acc = LeqAccumulator::new();
        assert_eq!(acc.samples(), 0);
        assert!(acc.leq_dbspl().is_infinite());
        assert!(acc.leq_dbspl().is_sign_negative());
    }
}
