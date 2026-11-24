//! 测试信号发生器。
//!
//! 手写线性同余发生器而**不引入 `rand`**：这里只需要可复现的伪随机数，
//! 而 `rand` 的版本演进会改变默认算法与流，让测试信号悄悄变掉。
//! 自带 LCG 保证同一个种子永远给出同一串数。
//!
//! 通过 `testgen` feature 对外开放，`noise-detect` 的测试用它构造合成特征流。

/// 64 位 LCG（Knuth MMIX 参数）。质量足够做噪声源，不是密码学安全的。
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1))
    }

    fn next_f64(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        // 取高 53 位映射到 [-1, 1)
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }
}

/// 正弦波。
///
/// * `freq_hz` —— 频率
/// * `fs` —— 采样率
/// * `n` —— 样本数
/// * `amplitude` —— 峰值幅度（1.0 = 满刻度）
pub fn sine(freq_hz: f64, fs: f64, n: usize, amplitude: f64) -> Vec<f64> {
    let w = std::f64::consts::TAU * freq_hz / fs;
    (0..n).map(|i| amplitude * (w * i as f64).sin()).collect()
}

/// 白噪声，均匀分布于 `[-amplitude, amplitude)`。
///
/// 同一个 `seed` 永远给出同一串样本。
pub fn white_noise(n: usize, amplitude: f64, seed: u64) -> Vec<f64> {
    let mut rng = Lcg::new(seed);
    (0..n).map(|_| rng.next_f64() * amplitude).collect()
}

/// 脉冲串。
///
/// * `rate_hz` —— 每秒脉冲数
/// * `width_samples` —— 每个脉冲持续的样本数（其余为静音）
pub fn impulse_train(
    fs: f64,
    n: usize,
    rate_hz: f64,
    width_samples: usize,
    amplitude: f64,
) -> Vec<f64> {
    let mut out = vec![0.0; n];
    let period = (fs / rate_hz).round() as usize;
    if period == 0 {
        return out;
    }
    let mut i = 0;
    while i < n {
        for k in 0..width_samples.min(n - i) {
            out[i + k] = amplitude;
        }
        i += period;
    }
    out
}

/// 稳态低频频带噪声——用来模拟空调/车流这类连续低频干扰，
/// 是检测器最重要的误报测试信号。
///
/// 白噪声过一个简单的一阶低通即可：调用方真正关心的是
/// 「连续 + 低频主导 + **没有上升沿**」，而非精确的频谱形状。
///
/// 注意**不是**「低峰值因数」——见
/// [`tests::frame_crest_cannot_separate_rumble_from_impulse_but_rise_can`]：
/// 10 ms 帧内这个信号的峰值因数与冲击几乎一样高。
pub fn low_frequency_rumble(fs: f64, n: usize, cutoff_hz: f64, amplitude: f64, seed: u64) -> Vec<f64> {
    let raw = white_noise(n, 1.0, seed);
    let alpha = 1.0 - (-std::f64::consts::TAU * cutoff_hz / fs).exp();
    let mut y = 0.0;
    let mut out: Vec<f64> = raw
        .iter()
        .map(|&x| {
            y += alpha * (x - y);
            y
        })
        .collect();

    // 归一化到指定幅度
    let peak = out.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    if peak > 0.0 {
        let g = amplitude / peak;
        for v in &mut out {
            *v *= g;
        }
    }
    out
}

/// 阶跃：前半段静音，后半段给定幅度。
pub fn step(n: usize, amplitude: f64) -> Vec<f64> {
    let mut out = vec![0.0; n];
    let half = n / 2;
    for v in &mut out[half..] {
        *v = amplitude;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::mean_square;

    #[test]
    fn sine_has_expected_mean_square() {
        let x = sine(1000.0, 48_000.0, 48_000, 1.0);
        assert!((mean_square(&x) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn sine_respects_amplitude() {
        let x = sine(100.0, 48_000.0, 48_000, 0.25);
        let peak = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!(peak <= 0.25 + 1e-12, "峰值 {peak} 超出幅度 0.25");
    }

    #[test]
    fn white_noise_is_reproducible_from_seed() {
        let a = white_noise(1000, 0.5, 42);
        let b = white_noise(1000, 0.5, 42);
        assert_eq!(a, b, "同一个种子必须给出同一串样本");
    }

    #[test]
    fn white_noise_differs_between_seeds() {
        let a = white_noise(1000, 0.5, 42);
        let b = white_noise(1000, 0.5, 43);
        assert_ne!(a, b);
    }

    #[test]
    fn white_noise_stays_within_amplitude() {
        let x = white_noise(10_000, 0.3, 7);
        assert!(x.iter().all(|v| v.abs() <= 0.3 + 1e-12));
    }

    #[test]
    fn impulse_train_has_expected_count() {
        // 1 Hz 持续 5 秒 → 5 个脉冲（在 0,1,2,3,4 秒处）
        let x = impulse_train(48_000.0, 48_000 * 5, 1.0, 48, 1.0);
        let nonzero = x.iter().filter(|v| **v != 0.0).count();
        assert_eq!(nonzero, 5 * 48, "非零样本数不符");
    }

    #[test]
    fn rumble_is_peak_normalized_and_low_frequency() {
        let x = low_frequency_rumble(48_000.0, 48_000, 30.0, 0.5, 3);
        let peak = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!((peak - 0.5).abs() < 1e-9, "峰值应被归一化到 0.5，实得 {peak}");
    }

    /// 10 ms 帧内的**最坏**峰值因数。
    fn worst_frame_crest_db(x: &[f64], fs: f64) -> f64 {
        let frame = (fs * 0.01) as usize;
        let skip = (fs * 0.1) as usize; // 跳过滤波器起步暂态
        x[skip..]
            .chunks(frame)
            .map(|c| {
                crate::metrics::crest_factor_db(
                    crate::metrics::peak_abs(c),
                    mean_square(c).sqrt(),
                )
            })
            .fold(0.0f64, f64::max)
    }

    /// 10 ms 帧的电平序列（dB），静音钳到 −120 dB 以免出现 −∞。
    fn frame_levels_db(x: &[f64], fs: f64) -> Vec<f64> {
        const FLOOR_DB: f64 = -120.0;
        let frame = (fs * 0.01) as usize;
        let skip = (fs * 0.1) as usize;
        x[skip..]
            .chunks(frame)
            .map(|c| {
                let ms = mean_square(c);
                if ms > 0.0 {
                    (10.0 * ms.log10()).max(FLOOR_DB)
                } else {
                    FLOOR_DB
                }
            })
            .collect()
    }

    /// 帧间最大跃升：当前帧相对**前 5 帧（50 ms）最大值**的增量。
    /// 这正是检测器的「上升沿」判据，也是这条测试要对比的对象。
    fn max_rise_db(levels: &[f64]) -> f64 {
        levels
            .windows(6)
            .map(|w| w[5] - w[..5].iter().cloned().fold(f64::NEG_INFINITY, f64::max))
            .fold(f64::NEG_INFINITY, f64::max)
    }

    /// **实测反直觉结果：10 ms 帧内的峰值因数无法区分稳态隆隆声和冲击。**
    ///
    /// 48 kHz 下的实测值：
    ///
    /// ```text
    /// 冲击  1 ms 宽 : 帧内峰值因数 10.00 dB   帧间上升 110.00 dB
    /// 冲击 10 ms 宽 : 帧内峰值因数  0.00 dB   帧间上升 120.00 dB
    /// 稳态隆隆声     : 帧内峰值因数  9.99 dB   帧间上升   4.60 dB
    /// ```
    ///
    /// 原因：63 Hz 的周期是 16 ms，**比 10 ms 的帧还长**，帧内装不下一个
    /// 完整周期，峰值因数于是退化成「这段波形碰巧落在周期的哪个相位」。
    /// 帧内峰值因数在 0–10 dB 之间乱跳，与信号是冲击还是连续无关。
    ///
    /// **结论：检测器必须依赖帧间上升沿，不能依赖帧内峰值因数。**
    /// 上升沿在两者上差了 100 dB 以上，判别力是压倒性的。
    ///
    /// 这条测试把结论连同原始数据一起钉住——将来若有人想把
    /// `min_crest_db` 当旋钮调来「修好」误报，会先撞到这里。
    #[test]
    fn frame_crest_cannot_separate_rumble_from_impulse_but_rise_can() {
        let fs = 48_000.0;
        let rumble = low_frequency_rumble(fs, 48_000, 30.0, 0.5, 3);
        let impulse = impulse_train(fs, 48_000, 2.0, 48, 1.0);

        let rumble_crest = worst_frame_crest_db(&rumble, fs);
        let impulse_crest = worst_frame_crest_db(&impulse, fs);
        let rumble_rise = max_rise_db(&frame_levels_db(&rumble, fs));
        let impulse_rise = max_rise_db(&frame_levels_db(&impulse, fs));

        // 1. 峰值因数：隆隆声的读数**不低**，判别特征不成立
        assert!(
            rumble_crest > 9.0,
            "稳态隆隆声的帧内峰值因数实测应约 10 dB（这是本测试的核心发现），\
             实得 {rumble_crest:.2} dB"
        );
        assert!(
            (rumble_crest - impulse_crest).abs() < 3.0,
            "峰值因数在两种信号上应几乎重叠，实测 {rumble_crest:.2} vs {impulse_crest:.2} dB"
        );

        // 2. 上升沿：判别力很强
        assert!(
            rumble_rise < 10.0,
            "稳态隆隆声没有上升沿，实得 {rumble_rise:.2} dB"
        );
        assert!(
            impulse_rise > 50.0,
            "冲击的上升沿应极显著，实得 {impulse_rise:.2} dB"
        );
        assert!(
            impulse_rise - rumble_rise > 40.0,
            "上升沿的判别间距应远大于峰值因数，实测 {:.1} dB",
            impulse_rise - rumble_rise
        );
    }

    #[test]
    fn step_is_silent_then_loud() {
        let x = step(1000, 0.8);
        assert!(x[..500].iter().all(|v| *v == 0.0));
        assert!(x[500..].iter().all(|v| *v == 0.8));
    }
}
