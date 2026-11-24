//! 二阶节级联。
//!
//! [`biquad`] crate 的 `Biquad` 是一个 **trait**（`run` / `reset_state` /
//! `update_coefficients`），具体实现是 `DirectForm2Transposed`。它的
//! `Coefficients<C>` 字段是公开的（`b0..b2`、`a1..a2`，归一化形式即 a0 = 1），
//! 所以我们可以直接用结构体字面量构造任意系数——A 计权不是 Butterworth/
//! 搁架形状，`Type::from_params` 表达不了它。
//!
//! 选用 **DF2T** 而非 DF1：定点/浮点下 DF2T 的舍入噪声更低，且状态变量更少。
//! 本项目的系数是设计时固定的，不需要在线重调。

use biquad::{Biquad, Coefficients, DirectForm2Transposed};

/// 一串二阶节。顺序有意义——先放动态范围大的节有利于数值稳定性，
/// 但这里的节都是精心配对的（每个极点对配最近的零点对），保持设计顺序即可。
#[derive(Clone, Debug)]
pub struct Cascade {
    sections: Vec<DirectForm2Transposed<f64>>,
    /// 保留一份原始系数。`DirectForm2Transposed` 不暴露内部系数，
    /// 而幅频评估需要它们。
    coeffs: Vec<Coefficients<f64>>,
}

impl Cascade {
    pub fn new(sections: Vec<Coefficients<f64>>) -> Self {
        let coeffs = sections.clone();
        Self {
            sections: sections
                .into_iter()
                .map(DirectForm2Transposed::<f64, f64>::new)
                .collect(),
            coeffs,
        }
    }

    /// 原地滤波。调用方负责预分配——本函数不分配。
    pub fn process_in_place(&mut self, x: &mut [f64]) {
        for section in &mut self.sections {
            for s in x.iter_mut() {
                *s = section.run(*s);
            }
        }
    }

    /// 清空滤波器状态。事件之间调用，避免上一段的尾音泄进下一段。
    pub fn reset(&mut self) {
        for section in &mut self.sections {
            section.reset_state();
        }
    }

    /// 把整体增益烘进第一个节的分子。
    ///
    /// 线性系统里缩放任意一个节的分子都等价于缩放整体，但作用在**第一个**
    /// 节可以让后续节的内部状态保持在与未缩放时相近的数量级。
    pub fn scale_gain(&mut self, gain: f64) {
        if let Some(c) = self.coeffs.first_mut() {
            c.b0 *= gain;
            c.b1 *= gain;
            c.b2 *= gain;
        }
        if let Some(s) = self.sections.first_mut() {
            *s = DirectForm2Transposed::<f64, f64>::new(self.coeffs[0]);
        }
    }

    /// 在数字频率 `f` 处的线性幅度响应，采样率 `fs`。
    ///
    /// 逐节计算 `|H_i(e^{jw})|` 再相乘。这里刻意不用 crate 的内部实现——
    /// 我们需要在**设计时**评估响应来做归一化，而那时滤波器还没跑过任何样本。
    pub fn magnitude_at(&self, f: f64, fs: f64) -> f64 {
        let w = std::f64::consts::TAU * f / fs;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();

        self.coeffs
            .iter()
            .map(|c| {
                let num_re = c.b0 + c.b1 * c1 + c.b2 * c2;
                let num_im = -(c.b1 * s1 + c.b2 * s2);
                let den_re = 1.0 + c.a1 * c1 + c.a2 * c2;
                let den_im = -(c.a1 * s1 + c.a2 * s2);

                let num = (num_re * num_re + num_im * num_im).sqrt();
                let den = (den_re * den_re + den_im * den_im).sqrt();
                // 分母为 0 只可能发生在滤波器不稳定时，而那由 tests/stability.rs
                // 单独拦截；这里返回 0 而不是 NaN，免得污染整条级联。
                if den <= 0.0 {
                    0.0
                } else {
                    num / den
                }
            })
            .product()
    }

    /// 线性幅度响应转 dB。
    pub fn magnitude_db_at(&self, f: f64, fs: f64) -> f64 {
        let m = self.magnitude_at(f, fs);
        if m <= 0.0 {
            f64::NEG_INFINITY
        } else {
            20.0 * m.log10()
        }
    }

    /// 所有极点是否严格位于单位圆内。
    ///
    /// 对 `1 + a1 z^-1 + a2 z^-2`，由 Jury 判据，二阶节的充要条件是
    /// `|a2| < 1` 且 `|a1| < 1 + a2`。系数在运行时按实际采样率生成，
    /// 所以这个检查必须在多个采样率下都成立——见 `tests/stability.rs`。
    pub fn is_stable(&self) -> bool {
        self.coeffs
            .iter()
            .all(|c| c.a2.abs() < 1.0 && c.a1.abs() < 1.0 + c.a2)
    }

    pub fn num_sections(&self) -> usize {
        self.sections.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单位增益直通节：b = [1,0,0]，a = [0,0]。
    fn pass_through() -> Coefficients<f64> {
        Coefficients {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }

    #[test]
    fn pass_through_preserves_signal() {
        let mut c = Cascade::new(vec![pass_through()]);
        let mut x = vec![0.5, -0.25, 1.0, 0.0, -1.0];
        let orig = x.clone();
        c.process_in_place(&mut x);
        assert_eq!(x, orig);
    }

    #[test]
    fn pass_through_has_unity_magnitude() {
        let c = Cascade::new(vec![pass_through()]);
        assert!((c.magnitude_at(1000.0, 48000.0) - 1.0).abs() < 1e-12);
        assert!((c.magnitude_db_at(1000.0, 48000.0)).abs() < 1e-9);
    }

    #[test]
    fn scale_gain_multiplies_response() {
        let mut c = Cascade::new(vec![pass_through()]);
        c.scale_gain(0.5);
        assert!((c.magnitude_at(440.0, 48000.0) - 0.5).abs() < 1e-12);
        // 幅度减半 = -6.02 dB
        assert!((c.magnitude_db_at(440.0, 48000.0) + 6.020_599_913_279_624).abs() < 1e-9);
    }

    #[test]
    fn reset_clears_state() {
        let coeffs = Coefficients {
            b0: 0.5,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        };
        let mut c = Cascade::new(vec![coeffs]);
        let mut x = vec![1.0; 8];
        c.process_in_place(&mut x);
        c.reset();
        let mut y = vec![1.0; 8];
        c.process_in_place(&mut y);
        // reset 后行为应与全新实例一致
        let mut fresh = Cascade::new(vec![coeffs]);
        let mut z = vec![1.0; 8];
        fresh.process_in_place(&mut z);
        assert_eq!(y, z);
    }

    #[test]
    fn unstable_section_is_detected() {
        // a2 = 1.5 ⇒ 极点跑到单位圆外
        let bad = Coefficients {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 1.5,
        };
        assert!(!Cascade::new(vec![bad]).is_stable());
        assert!(Cascade::new(vec![pass_through()]).is_stable());
    }
}
