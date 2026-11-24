//! 频率计权（IEC 61672-1）。
//!
//! # A 计权的数字实现
//!
//! 模拟原型：
//!
//! ```text
//! H_A(s) = K_A · s⁴ / [ (s+ω₁)² (s+ω₂) (s+ω₃) (s+ω₄)² ]
//! ```
//!
//! 极点频率（Hz）是**规范性**的，不要用圆整值：
//! f₁ = 20.598997（二重）、f₂ = 107.65265、f₃ = 737.86223、f₄ = 12194.217（二重）。
//! 常见的 `12200` 是个流传很广的错误。
//!
//! ## 6 个零点，不是 4 个
//!
//! `s → ∞` 时 `H_A ~ K_A/s²`，所以除了 4 个零点在原点，还有 **2 个零点在无穷远**。
//! 双线性变换把 `s = ∞` 映到 `z = -1`，再加上原点的 4 个零点映到 `z = +1`，
//! 数字域是：
//!
//! ```text
//! H_d(z) = g · (1 - z⁻¹)⁴ (1 + z⁻¹)² / ∏(1 - pᵢ z⁻¹)
//! ```
//!
//! 多出来的 `(1 + z⁻¹)²` 正是高频 −12 dB/倍频程滚降的来源；只用 4 个零点
//! 的分解式会让高频响应完全错掉。验算：低频端 `(1-z⁻¹)⁴` 给出 +24 dB/oct
//! 上升，高频端 `(1+z⁻¹)²` 给出 −12 dB/oct 下降，与 IEC 曲线一致。
//!
//! ## 极点映射与分解
//!
//! 双线性：`p = (1 - x)/(1 + x)`，其中 `x = ω·T/2`。实极点映到实极点。
//! 分解为三个二阶节，每个极点对配就近的零点对：
//!
//! | 节 | 分子 | 分母 |
//! |---|---|---|
//! | 1 | `(1 - z⁻¹)²` | `(1 - p₁z⁻¹)²` |
//! | 2 | `(1 + z⁻¹)²` | `(1 - p₂z⁻¹)(1 - p₃z⁻¹)` |
//! | 3 | `(1 - z⁻¹)²` | `(1 - p₄z⁻¹)²` |
//!
//! ## 归一化
//!
//! 最后**数值**归一化到 1 kHz 处 0 dB。不硬编码 `K_A = 7.39705e9`——
//! 那个常数是按圆整极点算出来的，与实际使用的极点集存在约 0.1% 的偏差，
//! 数值归一化一次性消除掉这个差异以及双线性变换引入的任何整体增益。

use crate::cascade::Cascade;
use biquad::Coefficients;
use std::f64::consts::TAU;

/// A 计权极点频率（Hz），IEC 61672-1。**不要圆整。**
pub const A_POLE_FREQS_HZ: [f64; 4] = [20.598_997, 107.652_65, 737.862_23, 12_194.217];

/// C 计权极点频率（Hz）。C 计权在中低频近乎平坦（63 Hz 处仅 −0.8 dB），
/// 因此比 A 计权更适合做低频评估的报告量。
pub const C_POLE_FREQS_HZ: [f64; 2] = [20.598_997, 12_194.217];

/// 归一化频率——1 kHz 处定义为 0 dB，这正是选 1 kHz 的原因。
pub const NORMALIZATION_FREQ_HZ: f64 = 1000.0;

/// 频率计权的通用接口。
pub trait Weighting {
    fn process_in_place(&mut self, x: &mut [f64]);
    fn reset(&mut self);
    /// 该计权在频率 `f` 处的响应（dB）。
    fn magnitude_db_at(&self, f: f64, fs: f64) -> f64;
    /// 所有极点是否严格在单位圆内。
    ///
    /// 这不是学术洁癖：A 计权的最高极点 12194 Hz 在 22.05 kHz 及以下的
    /// 采样率下会越过 `0.45·fs`，双线性映射把它折到接近 Nyquist 的位置。
    /// 滤波器仍然稳定（`|p| < 1` 对一切有限 f 都成立），但高频响应已经
    /// 不是 A 计权曲线了。所以稳定性检查要配合 [`Self::is_degraded`]
    /// 一起看——**只查稳定会漏掉这个失效模式**。
    fn is_stable(&self) -> bool;

    /// 是否有极点的规范频率越过了 `0.45·fs`。
    ///
    /// # 这个标记到底在警告什么（实测，勿凭直觉解读）
    ///
    /// 它**不是**「低频不准了」。实测扫描（见 `tests/stability.rs`）显示
    /// 20 Hz–2 kHz 的偏差在**所有**支持采样率下都不超过 0.28 dB，包括
    /// 8 kHz 采样率。本项目报告的噪声能量全部落在这个频段。
    ///
    /// 它警告的是**高频段被双线性畸变压坏**。8 kHz 处的实测偏差：
    ///
    /// | fs | 8 kHz 处偏差 |
    /// |---|---|
    /// | 22 050 | +5.12 dB |
    /// | 32 000 | +1.53 dB |
    /// | 48 000 | +0.59 dB |
    ///
    /// 实际后果：若声源含显著高频能量（说话、电视），A 计权**总级**会被
    /// 低估；而八度带 1 kHz 及以下的读数不受影响。UI 的警告文案要照这个
    /// 说，不要说成「计权不可信」。
    fn is_degraded(&self) -> bool;
}

/// 双线性映射一个模拟实极点 `s = -ω` 到 z 平面。
#[inline]
fn bilinear_pole(freq_hz: f64, fs: f64) -> f64 {
    let x = TAU * freq_hz / fs / 2.0;
    (1.0 - x) / (1.0 + x)
}

/// 构造并归一化一条计权级联。
fn normalized_cascade(sections: Vec<Coefficients<f64>>, fs: f64) -> Cascade {
    let mut cascade = Cascade::new(sections);
    let raw = cascade.magnitude_at(NORMALIZATION_FREQ_HZ, fs);
    // 只有在滤波器失稳时 raw 才可能是 0/NaN。保持 1.0 而不是制造 inf，
    // 让上游的稳定性测试去报告真正的问题。
    if raw.is_finite() && raw > 0.0 {
        cascade.scale_gain(1.0 / raw);
    }
    cascade
}

/// 极点集里是否有频率越过了 `0.45·fs`。
fn poles_degraded(pole_freqs: &[f64], fs: f64) -> bool {
    let limit = fs * crate::design::MAX_NORMALIZED_FREQ;
    pole_freqs.iter().any(|f| *f >= limit)
}

/// A 计权滤波器。
#[derive(Clone, Debug)]
pub struct AWeighting {
    cascade: Cascade,
    degraded: bool,
}

impl AWeighting {
    pub fn new(fs: f64) -> Self {
        let [f1, f2, f3, f4] = A_POLE_FREQS_HZ;
        let (p1, p2, p3, p4) = (
            bilinear_pole(f1, fs),
            bilinear_pole(f2, fs),
            bilinear_pole(f3, fs),
            bilinear_pole(f4, fs),
        );

        let sections = vec![
            // 节 1：低频拐点。分子 (1-z⁻¹)² = 1 - 2z⁻¹ + z⁻²
            Coefficients {
                b0: 1.0,
                b1: -2.0,
                b2: 1.0,
                a1: -2.0 * p1,
                a2: p1 * p1,
            },
            // 节 2：中频。分子 (1+z⁻¹)² = 1 + 2z⁻¹ + z⁻² ——
            // 这两个 +1 的零点是无穷远零点经双线性变换的像，不可省略。
            Coefficients {
                b0: 1.0,
                b1: 2.0,
                b2: 1.0,
                a1: -(p2 + p3),
                a2: p2 * p3,
            },
            // 节 3：高频拐点
            Coefficients {
                b0: 1.0,
                b1: -2.0,
                b2: 1.0,
                a1: -2.0 * p4,
                a2: p4 * p4,
            },
        ];

        Self {
            cascade: normalized_cascade(sections, fs),
            degraded: poles_degraded(&A_POLE_FREQS_HZ, fs),
        }
    }

    pub fn magnitude_db_at(&self, f: f64, fs: f64) -> f64 {
        self.cascade.magnitude_db_at(f, fs)
    }
}

impl Weighting for AWeighting {
    fn process_in_place(&mut self, x: &mut [f64]) {
        self.cascade.process_in_place(x);
    }
    fn reset(&mut self) {
        self.cascade.reset();
    }
    fn magnitude_db_at(&self, f: f64, fs: f64) -> f64 {
        self.cascade.magnitude_db_at(f, fs)
    }
    fn is_stable(&self) -> bool {
        self.cascade.is_stable()
    }
    fn is_degraded(&self) -> bool {
        self.degraded
    }
}

/// C 计权滤波器。
///
/// `H_C(s) = K_C s² / [(s+ω₁)² (s+ω₄)²]`。两个零点在原点、两个在无穷远，
/// 所以数字域是 `(1-z⁻¹)²(1+z⁻¹)²`。
#[derive(Clone, Debug)]
pub struct CWeighting {
    cascade: Cascade,
    degraded: bool,
}

impl CWeighting {
    pub fn new(fs: f64) -> Self {
        let [f1, f4] = C_POLE_FREQS_HZ;
        let (p1, p4) = (bilinear_pole(f1, fs), bilinear_pole(f4, fs));
        let sections = vec![
            Coefficients {
                b0: 1.0,
                b1: -2.0,
                b2: 1.0,
                a1: -2.0 * p1,
                a2: p1 * p1,
            },
            Coefficients {
                b0: 1.0,
                b1: 2.0,
                b2: 1.0,
                a1: -2.0 * p4,
                a2: p4 * p4,
            },
        ];
        Self {
            cascade: normalized_cascade(sections, fs),
            degraded: poles_degraded(&C_POLE_FREQS_HZ, fs),
        }
    }

    pub fn magnitude_db_at(&self, f: f64, fs: f64) -> f64 {
        self.cascade.magnitude_db_at(f, fs)
    }
}

impl Weighting for CWeighting {
    fn process_in_place(&mut self, x: &mut [f64]) {
        self.cascade.process_in_place(x);
    }
    fn reset(&mut self) {
        self.cascade.reset();
    }
    fn magnitude_db_at(&self, f: f64, fs: f64) -> f64 {
        self.cascade.magnitude_db_at(f, fs)
    }
    fn is_stable(&self) -> bool {
        self.cascade.is_stable()
    }
    fn is_degraded(&self) -> bool {
        self.degraded
    }
}

/// Z 计权（"零"计权）：按定义就是**不做任何滤波**。
///
/// IEC 61672-1:2002 引入它以取代含义模糊的 "Linear"/"Flat"。标准里的
/// ±1.5 dB 平坦度容差是对**整机**（含麦克风）的要求，不是软件要实现的
/// 传递函数——软件这边就是直通。
#[derive(Clone, Copy, Debug, Default)]
pub struct ZWeighting;

impl Weighting for ZWeighting {
    fn process_in_place(&mut self, _x: &mut [f64]) {}
    fn reset(&mut self) {}
    fn magnitude_db_at(&self, _f: f64, _fs: f64) -> f64 {
        0.0
    }
    /// 没有极点，恒稳且恒不降级——这是 Z 计权在低采样率下唯一可靠的计权。
    fn is_stable(&self) -> bool {
        true
    }
    fn is_degraded(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 48_000.0;

    /// IEC 61672-1 Table 1/3 的 A 计权标称值，单位 dB。
    /// 这是**规范值**，不是从某个公式算出来的——测试必须对着它，
    /// 否则就是在拿自己的实现验证自己。
    const IEC_A_TABLE: &[(f64, f64)] = &[
        (20.0, -50.5),
        (31.5, -39.4),
        (63.0, -26.2),
        (100.0, -19.1),
        (125.0, -16.1),
        (250.0, -8.6),
        (500.0, -3.2),
        (1000.0, 0.0),
        (2000.0, 1.2),
        (4000.0, 1.0),
        (8000.0, -1.1),
    ];

    /// 双线性变换在接近 Nyquist 处会压缩频率轴。48 kHz 采样率下，数字域的
    /// 8 kHz 对应模拟原型上的约 8.82 kHz，而 A 计权曲线在该处斜率约
    /// −0.5 dB/kHz，于是产生约 0.55 dB 的偏差。
    ///
    /// **这是双线性变换的固有性质，不是实现缺陷。** 把每个极点精确锚定在
    /// 它自己的规范频率上（本实现的做法）会让 20 Hz–4 kHz 这个**取证主力
    /// 频段**的误差小到可以忽略，代价就是把偏差推到接近 Nyquist 的高频端。
    /// 换一个预畸变锚点只会把误差搬到别处，不会消掉它。
    ///
    /// 下面因此分两段断言：4 kHz 及以下用 ±0.5 dB，8 kHz 单独用 ±0.7 dB。
    /// 放宽不是因为测不过，而是因为上面那段推导说明 0.5 dB 在那个频点上
    /// 是双线性变换达不到的——写清楚原因，而不是把容差悄悄调大。
    const BILINEAR_WARP_TOLERANCE_DB: f64 = 0.7;

    #[test]
    fn a_weighting_matches_iec_table() {
        let w = AWeighting::new(FS);
        for &(f, expected) in IEC_A_TABLE {
            let got = w.magnitude_db_at(f, FS);
            let tol = if f > 4_000.0 {
                BILINEAR_WARP_TOLERANCE_DB
            } else {
                0.5
            };
            assert!(
                (got - expected).abs() < tol,
                "A 计权 @ {f} Hz：期望 {expected} dB，实得 {got:.3} dB（容差 {tol}）"
            );
        }
    }

    #[test]
    fn a_weighting_is_zero_db_at_1khz() {
        let w = AWeighting::new(FS);
        assert!(w.magnitude_db_at(1000.0, FS).abs() < 1e-6);
    }

    /// C 计权标称值，IEC 61672-1。
    #[test]
    fn c_weighting_matches_iec_table() {
        let w = CWeighting::new(FS);
        let table = [
            (20.0, -6.2),
            (31.5, -3.0),
            (63.0, -0.8),
            (100.0, -0.3),
            (125.0, -0.2),
            (1000.0, 0.0),
        ];
        for (f, expected) in table {
            let got = w.magnitude_db_at(f, FS);
            assert!(
                (got - expected).abs() < 0.5,
                "C 计权 @ {f} Hz：期望 {expected} dB，实得 {got:.3} dB"
            );
        }
    }

    /// 这条测试锁住那个最容易犯的错误：漏掉无穷远零点。
    ///
    /// 若分子只有 `(1-z⁻¹)⁴` 而没有 `(1+z⁻¹)²`，高频端会变成滚升而不是
    /// 滚降——8 kHz 处的读数会偏离标称值好几个 dB。
    #[test]
    fn a_weighting_rolls_off_at_high_frequency() {
        let w = AWeighting::new(FS);
        let at_8k = w.magnitude_db_at(8000.0, FS);
        assert!(
            at_8k < 0.5,
            "8 kHz 处应为负值（高频滚降），实得 {at_8k:.3} dB —— \
             怀疑 (1+z⁻¹)² 零点被漏掉了"
        );
    }

    #[test]
    fn a_weighting_rises_at_24db_per_octave_below_20hz() {
        let w = AWeighting::new(FS);
        let a = w.magnitude_db_at(5.0, FS);
        let b = w.magnitude_db_at(10.0, FS);
        // 4 个原点零点给出 +24 dB/oct
        assert!(
            ((b - a) - 24.0).abs() < 2.0,
            "20 Hz 以下应约 +24 dB/oct，实得 {:.2}",
            b - a
        );
    }

    #[test]
    fn z_weighting_is_exactly_passthrough() {
        let mut w = ZWeighting;
        let orig = vec![0.1, -0.9, 0.0, 1.0];
        let mut x = orig.clone();
        w.process_in_place(&mut x);
        assert_eq!(x, orig);
        assert_eq!(w.magnitude_db_at(123.0, FS), 0.0);
    }

    /// 直流必须被完全挡掉——A 计权有 4 个零点在 z = 1。
    /// 如果这条不过，说明节的分子配错了，而直流泄漏会污染所有低频读数。
    #[test]
    fn a_weighting_rejects_dc() {
        let w = AWeighting::new(FS);
        let dc = w.magnitude_db_at(0.0, FS);
        assert!(dc < -200.0, "直流应被完全抑制，实得 {dc} dB");
    }
}
