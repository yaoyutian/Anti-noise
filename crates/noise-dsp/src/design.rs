//! 滤波器系数设计。
//!
//! 全部采用 **RBJ cookbook 的预畸变双线性变换**形式。RBJ 公式里的
//! `w0 = 2πf₀/fs` 已经隐含了预畸变——它把模拟原型的拐点精确映射到数字
//! 域的 `f₀`，而不是像朴素双线性那样在接近 Nyquist 时把拐点压低。
//!
//! # 为什么所有设计函数都接受 `fs`
//!
//! 系数必须是**设备实际采样率**的函数，不能写死 48 kHz。原因有三：
//!
//! 1. WASAPI shared 模式会静默重采样，你请求的采样率和实际拿到的可能不同；
//! 2. 蓝牙设备会掉到 8/16 kHz；
//! 3. 引入重采样器来凑 48 kHz 会带来自己的通带纹波，破坏「1 kHz 处 0 dB」
//!    这个校准基准。
//!
//! 所以：**读回实际达成的配置，据此设计滤波器，不加重采样器。**

use biquad::Coefficients;
use std::f64::consts::TAU;

/// 设计频率相对采样率的硬上限。
///
/// 超过这个比例时 `sin(w0)` 会趋近甚至越过零点，`alpha` 变负，滤波器直接
/// 失稳。0.45 留出了足够余量（对应 0.9 × Nyquist）。
pub const MAX_NORMALIZED_FREQ: f64 = 0.45;

/// 一套设计好的节，附带是否发生了降级钳位。
#[derive(Clone, Debug)]
pub struct Design {
    pub sections: Vec<Coefficients<f64>>,
    /// 为真表示某个设计频率被钳到了 [`MAX_NORMALIZED_FREQ`]。
    /// 这个标记要一路传播到 session manifest 和 UI 警告——静默降级比降级本身更糟。
    pub degraded: bool,
}

/// Butterworth 归一化极点 Q 值，由 `Q_k = 1 / (2 sin(π(2k+1)/(2N)))` 导出。
///
/// 从公式算而不是查表：查表抄错一位没人看得出来，公式不会。
pub fn butterworth_q_values(order: usize) -> Vec<f64> {
    assert!(order > 0, "滤波器阶数必须为正");
    let mut qs: Vec<f64> = (0..order / 2)
        .map(|k| {
            let theta = std::f64::consts::PI * (2 * k + 1) as f64 / (2 * order) as f64;
            1.0 / (2.0 * theta.sin())
        })
        .collect();
    // 公式给出的顺序是 Q 递减的；排成升序以对应公开的 Butterworth Q 表。
    qs.sort_by(|a, b| a.partial_cmp(b).expect("Q 值不可能为 NaN"));
    qs
}

/// 把设计频率钳进安全区间。
fn clamp_freq(f0: f64, fs: f64) -> (f64, bool) {
    let max = fs * MAX_NORMALIZED_FREQ;
    if f0 >= max {
        (max, true)
    } else {
        (f0.max(1e-6), false)
    }
}

/// RBJ 二阶低通。
fn lowpass_section(fs: f64, f0: f64, q: f64) -> Coefficients<f64> {
    let w0 = TAU * f0 / fs;
    let (sin_w0, cos_w0) = w0.sin_cos();
    let alpha = sin_w0 / (2.0 * q);
    let a0 = 1.0 + alpha;
    Coefficients {
        b0: ((1.0 - cos_w0) / 2.0) / a0,
        b1: (1.0 - cos_w0) / a0,
        b2: ((1.0 - cos_w0) / 2.0) / a0,
        a1: (-2.0 * cos_w0) / a0,
        a2: (1.0 - alpha) / a0,
    }
}

/// RBJ 二阶高通。
fn highpass_section(fs: f64, f0: f64, q: f64) -> Coefficients<f64> {
    let w0 = TAU * f0 / fs;
    let (sin_w0, cos_w0) = w0.sin_cos();
    let alpha = sin_w0 / (2.0 * q);
    let a0 = 1.0 + alpha;
    Coefficients {
        b0: ((1.0 + cos_w0) / 2.0) / a0,
        b1: (-(1.0 + cos_w0)) / a0,
        b2: ((1.0 + cos_w0) / 2.0) / a0,
        a1: (-2.0 * cos_w0) / a0,
        a2: (1.0 - alpha) / a0,
    }
}

/// N 阶 Butterworth 低通。
pub fn butterworth_lowpass(fs: f64, f0: f64, order: usize) -> Design {
    let (f0, degraded) = clamp_freq(f0, fs);
    let sections = butterworth_q_values(order)
        .into_iter()
        .map(|q| lowpass_section(fs, f0, q))
        .collect();
    Design { sections, degraded }
}

/// N 阶 Butterworth 高通。
pub fn butterworth_highpass(fs: f64, f0: f64, order: usize) -> Design {
    let (f0, degraded) = clamp_freq(f0, fs);
    let sections = butterworth_q_values(order)
        .into_iter()
        .map(|q| highpass_section(fs, f0, q))
        .collect();
    Design { sections, degraded }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cascade::Cascade;

    #[test]
    fn butterworth_q_values_match_known_orders() {
        // 2 阶：单个 Q = 1/√2
        let q2 = butterworth_q_values(2);
        assert_eq!(q2.len(), 1);
        assert!((q2[0] - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-12);

        // 4 阶：经典的 0.5412 / 1.3066，**升序**排列以对应公开的 Q 表，
        // 方便拿代码跟参考资料逐位核对。级联里各节的顺序不影响总响应
        // （LTI 可交换），但会影响中间节点的数值范围，而这里无关紧要。
        let q4 = butterworth_q_values(4);
        assert_eq!(q4.len(), 2);
        assert!((q4[0] - 0.541_196_100_146_197).abs() < 1e-12, "{q4:?}");
        assert!((q4[1] - 1.306_562_964_876_377).abs() < 1e-12, "{q4:?}");
        assert!(q4[0] < q4[1], "Q 值应按升序排列，实得 {q4:?}");
    }

    #[test]
    fn lowpass_minus_3db_at_cutoff() {
        let fs = 48000.0;
        for order in [2, 4] {
            let d = butterworth_lowpass(fs, 1000.0, order);
            let c = Cascade::new(d.sections);
            // 二阶节的 Q 值在拐点处给出 1/√2，级联后仍是 -3 dB
            let db = c.magnitude_db_at(1000.0, fs);
            assert!(
                (db + 3.010_299_956_639_812).abs() < 1e-6,
                "{order} 阶低通在拐点应为 -3.01 dB，实得 {db}"
            );
        }
    }

    #[test]
    fn highpass_minus_3db_at_cutoff() {
        let fs = 48000.0;
        for order in [2, 4] {
            let d = butterworth_highpass(fs, 1000.0, order);
            let c = Cascade::new(d.sections);
            let db = c.magnitude_db_at(1000.0, fs);
            assert!(
                (db + 3.010_299_956_639_812).abs() < 1e-6,
                "{order} 阶高通在拐点应为 -3.01 dB，实得 {db}"
            );
        }
    }

    #[test]
    fn stopband_slope_matches_order() {
        let fs = 48000.0;
        // 拐点取 100 Hz 而测量点取 400/800 Hz：必须让整个测量区间远离
        // Nyquist，否则双线性变换的频率压缩会**抬高表观斜率**
        // （48 kHz 下用 500 Hz 拐点测 4k/8k 会得到 13.3 dB/oct 而不是 12）。
        // 那测的是频率轴畸变，不是滤波器阶数。
        for (order, expected_db_per_octave) in [(2usize, 12.0), (4, 24.0)] {
            let d = butterworth_lowpass(fs, 100.0, order);
            let c = Cascade::new(d.sections);
            let a = c.magnitude_db_at(400.0, fs);
            let b = c.magnitude_db_at(800.0, fs);
            let slope = a - b;
            assert!(
                (slope - expected_db_per_octave).abs() < 1.0,
                "{order} 阶阻带斜率应约 {expected_db_per_octave} dB/oct，实得 {slope}"
            );
        }
    }

    #[test]
    fn design_is_stable_at_all_supported_rates() {
        for fs in [16_000.0, 22_050.0, 32_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            assert!(Cascade::new(butterworth_lowpass(fs, 200.0, 4).sections).is_stable());
            assert!(Cascade::new(butterworth_highpass(fs, 25.0, 4).sections).is_stable());
        }
    }

    #[test]
    fn over_nyquist_design_is_clamped_and_flagged_degraded() {
        let fs = 16000.0;
        // 8 kHz 远高于 0.45 × 16 kHz = 7.2 kHz
        let d = butterworth_lowpass(fs, 8000.0, 2);
        assert!(d.degraded, "超限设计必须置 degraded 标记");
        assert!(Cascade::new(d.sections).is_stable(), "钳位后必须仍然稳定");
    }

    #[test]
    fn in_range_design_is_not_flagged_degraded() {
        let d = butterworth_lowpass(48000.0, 1000.0, 2);
        assert!(!d.degraded);
    }
}
