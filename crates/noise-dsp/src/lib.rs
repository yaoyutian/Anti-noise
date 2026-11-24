//! 纯 DSP。
//!
//! 本 crate **不依赖 cpal、不碰 I/O、不创建线程**。
//! 这条边界是刻意用 crate 依赖关系强制的：一个链接不到 cpal 的 crate
//! 在编译期就不可能违反实时约束，比代码评审可靠。
//!
//! 全部计算使用 `f64`。A 计权的双极点贴近单位圆（z ≈ 0.9973）而双零点
//! 在 z = 1.0，f32 下这对抵消的系数舍入误差会显著抬高 20 Hz 附近的噪声底，
//! 直接影响「−50 dB @ 20 Hz」这个指标的准确性。48 kHz 的吞吐量不值一提。

pub mod cascade;

/// 提前把 `biquad` 的系数类型暴露出来。
///
/// 下游（`noise-detect` 的离线提取、将来的频响绘图）需要它来接收
/// [`design`] 返回的系数，但没有理由为此直接依赖 `biquad` ——
/// 那样 `biquad` 的版本就从一个内部实现细节变成了公开的 API 契约。
pub use biquad::Coefficients;
pub mod calibration;
pub mod design;
pub mod features;
pub mod metrics;
pub mod octave;
pub mod weighting;

/// 测试信号发生器。
///
/// 除了测试，`noise-detect` 的场景测试与 `--replay` 工具也用它构造合成输入，
/// 因此通过 feature 对外开放；本 crate 自己的测试则无条件可见。
#[cfg(any(test, feature = "testgen"))]
pub mod testgen;

/// 单位重导出。领域词汇的真相在 `noise-core`，这里只是让调用方少写一个 `use`。
pub mod units {
    pub use noise_core::units::*;
    pub use noise_core::P0;
}

pub use calibration::Calibration;
pub use cascade::Cascade;
pub use features::{spectral_flatness_db, FrameFeatures};
pub use metrics::{crest_factor_db, mean_square, peak_abs, LeqAccumulator, SquaredEwma, TimeWeighting};
pub use octave::OctaveBank;
pub use weighting::{AWeighting, CWeighting, ZWeighting};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsp_crate_reexports_core_units() {
        // 冒烟测试：确认 crate 图与 workspace 依赖解析正确。
        //
        // 这里用区间断言而不是复述精确字面量——本测试要验证的是
        // re-export 通了，不是重新推导 1 Pa = 94 dB SPL。抄一遍字面量
        // 只会把 noise-core 的期望值再抄错一次，而那种错误在两边
        // 同时出错时是看不出来的。
        let dbspl = units::mean_square_to_dbspl(1.0).0;
        assert!(
            (dbspl - 94.0).abs() < 0.1,
            "1 Pa RMS 应约为 94 dB SPL，实得 {dbspl}"
        );
    }
}
