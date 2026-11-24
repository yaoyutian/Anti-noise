//! 带单位的 newtype，防止 dB 运算接错。
//!
//! 这些类型刻意**不**实现 `Add`/`Sub`——两个 dB 值相加在声学上没有意义
//! （要相加的是能量，不是电平）。需要合并时用 [`DbSpl::energy_sum`] 这类
//! 显式方法，让意图在调用点可见。

use serde::{Deserialize, Serialize};

/// 数字满量程相对电平。0 dBFS 是 ADC 满刻度，恒为负值（或 0）。
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Dbfs(pub f64);

/// 声压级，相对 20 µPa。**只有在做过校准之后才有绝对意义。**
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DbSpl(pub f64);

/// A 计权声压级。数值本身已包含计权，不要重复施加。
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DbA(pub f64);

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Hertz(pub f64);

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Milliseconds(pub f64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Samples(pub u64);

/// 线性均方值 → dB（相对给定参考）。
///
/// 用 `10*log10` 而非 `20*log10`，因为输入是**能量**量。全刻度正弦
/// （幅度 1.0）的均方值是 0.5，对应 −3.01 dBFS——这个 3 dB 是
/// dBFS 与 dB SPL 换算时最经典的一个坑。
#[inline]
pub fn mean_square_to_db(mean_square: f64, reference: f64) -> f64 {
    // 钳到极小正数，避免 log10(0) 产生 -inf 污染下游统计与 JSON 序列化。
    const FLOOR: f64 = f64::MIN_POSITIVE;
    10.0 * (mean_square.max(FLOOR) / (reference * reference)).log10()
}

/// 线性均方值 → 相对 [`crate::P0`] 的 dB SPL。
#[inline]
pub fn mean_square_to_dbspl(mean_square: f64) -> DbSpl {
    DbSpl(mean_square_to_db(mean_square, crate::P0))
}

/// 线性均方值 → dBFS（满刻度为 1.0）。
#[inline]
pub fn mean_square_to_dbfs(mean_square: f64) -> Dbfs {
    Dbfs(mean_square_to_db(mean_square, 1.0))
}

impl DbSpl {
    /// 按能量（而非电平）求和。`&[dB, dB]` → 合并后的 dB。
    ///
    /// 两个相同声源相加得到 +3.01 dB，不是 +2×。这个方法是唯一
    /// 被允许的 dB 合并途径。
    pub fn energy_sum(levels: &[DbSpl]) -> Option<DbSpl> {
        if levels.is_empty() {
            return None;
        }
        let sum: f64 = levels.iter().map(|l| 10f64.powf(l.0 / 10.0)).sum();
        Some(DbSpl(10.0 * sum.log10()))
    }
}

impl DbA {
    /// 见 [`DbSpl::energy_sum`]。
    pub fn energy_sum(levels: &[DbA]) -> Option<DbA> {
        if levels.is_empty() {
            return None;
        }
        let sum: f64 = levels.iter().map(|l| 10f64.powf(l.0 / 10.0)).sum();
        Some(DbA(10.0 * sum.log10()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_scale_sine_is_minus_three_dbfs() {
        // 幅度 1.0 的正弦均方值为 0.5。
        let d = mean_square_to_dbfs(0.5);
        assert!((d.0 - (-3.010_299_956_639_812)).abs() < 1e-9, "{d:?}");
    }

    #[test]
    fn doubling_amplitude_is_plus_six_db() {
        let a = mean_square_to_dbfs(0.25);
        let b = mean_square_to_dbfs(1.0); // 幅度翻倍 → 均方 ×4
        assert!((b.0 - a.0 - 6.020_599_913_279_624).abs() < 1e-9);
    }

    #[test]
    fn one_pascal_rms_is_94_dbspl() {
        // 1 Pa RMS 定义为 94 dB SPL —— 声校准器的标准输出。
        // 精确值 = 10*log10(1 / (2e-5)^2) = 10*log10(2.5e9) = 93.97940008672037
        let d = mean_square_to_dbspl(1.0);
        assert!((d.0 - 93.979_400_086_720_37).abs() < 1e-9, "{d:?}");
    }

    #[test]
    fn energy_sum_of_two_equal_sources_is_plus_three() {
        let s = DbSpl::energy_sum(&[DbSpl(60.0), DbSpl(60.0)]).unwrap();
        assert!((s.0 - 63.010_299_956_639_81).abs() < 1e-9, "{s:?}");
    }

    #[test]
    fn zero_energy_does_not_produce_infinity() {
        let d = mean_square_to_dbspl(0.0);
        assert!(d.0.is_finite(), "静音输入产生了 {d:?}");
    }
}
