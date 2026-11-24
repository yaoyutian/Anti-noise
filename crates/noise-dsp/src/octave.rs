//! 八度带分析。
//!
//! 用 IIR 滤波器组而非 FFT，理由是延迟：FFT 的频带能量至少要等一整个窗口
//! 才出得来，而低频分辨率要求长窗口——20 Hz 的 1 个周期就是 50 ms。
//! IIR 是逐样本的，几乎没有额外延迟，这正是触发路径需要的。
//!
//! # 精度声明
//!
//! 这是**近似的**八度带，**不是 IEC 61260 合规**的滤波器组。每带用
//! 4 阶 Butterworth 高通 + 4 阶 Butterworth 低通串联，裙边比标准的
//! Class 1 要求缓。它足以回答「低频段有没有能量」和「这一带的频谱长什么样」，
//! 但**不能**用于出具合规的频谱报告。
//!
//! 每带在设计时按其在中心频率处的实际响应做了功率归一化，所以各带读数
//! 在中心频率上是一致的、可直接横向比较。

use crate::cascade::Cascade;
use crate::design::{butterworth_highpass, butterworth_lowpass};
use crate::metrics::mean_square;
use noise_core::units::mean_square_to_db;

/// 默认分析带：31.5 Hz 到 1 kHz 六个倍频程。
///
/// 下限取 31.5 而非 16 是因为笔记本麦克风在 20 Hz 附近基本没有可用响应，
/// 再往下加带只会显示拾音硬件噪声。上限取 1 kHz 是取证够用且不引语音干扰。
pub const DEFAULT_CENTERS_HZ: [f64; 6] = [31.5, 63.0, 125.0, 250.0, 500.0, 1000.0];

/// 每侧滤波器的阶数。4 阶给出 24 dB/oct 的裙边，
/// 足以让 1 kHz 在 31.5 Hz 带里衰减 50 dB 以上。
const ORDER: usize = 4;

struct Band {
    cascade: Cascade,
    /// 1 / |H(fc)|²——把中心频率处的设计损耗补回来。
    /// 高通和低通各自在 fc 处贡献约 −1 dB，不补的话每带读数都偏低约 2 dB。
    power_gain: f64,
}

/// 一组八度带滤波器。
pub struct OctaveBank {
    bands: Vec<Band>,
    /// 复用的滤波临时缓冲，避免每帧分配。
    scratch: Vec<f64>,
    /// 复用的均方值缓冲，供 [`Self::process_db`] 使用。
    ms_scratch: Vec<f64>,
    /// 任一带的设计频率被钳位过。要一路传播到 UI 警告——
    /// 静默降级比降级本身更糟。
    degraded: bool,
}

impl OctaveBank {
    pub fn new(fs: f64, centers_hz: &[f64]) -> Self {
        let mut degraded = false;

        let bands = centers_hz
            .iter()
            .map(|&fc| {
                // 一个倍频程带：下拐点 fc/√2，上拐点 fc·√2
                let f_low = fc / std::f64::consts::SQRT_2;
                let f_high = fc * std::f64::consts::SQRT_2;

                let hp = butterworth_highpass(fs, f_low, ORDER);
                let lp = butterworth_lowpass(fs, f_high, ORDER);
                degraded |= hp.degraded || lp.degraded;

                let mut sections = hp.sections;
                sections.extend(lp.sections);

                let cascade = Cascade::new(sections);
                let mag = cascade.magnitude_at(fc, fs);
                let power_gain = if mag > 0.0 { 1.0 / (mag * mag) } else { 1.0 };

                Band {
                    cascade,
                    power_gain,
                }
            })
            .collect();

        Self {
            bands,
            scratch: Vec::new(),
            ms_scratch: vec![0.0; centers_hz.len()],
            degraded,
        }
    }

    /// 用默认带组构造。
    pub fn with_default_bands(fs: f64) -> Self {
        Self::new(fs, &DEFAULT_CENTERS_HZ)
    }

    pub fn num_bands(&self) -> usize {
        self.bands.len()
    }

    /// 是否有带的设计频率被钳位（采样率过低）。
    pub fn is_degraded(&self) -> bool {
        self.degraded
    }

    /// 计算每个带的均方值（已按中心频率归一化）。
    ///
    /// `out` 长度必须等于带数。调用方预分配；本函数只在块变大时
    /// 让 `scratch` 增长一次，稳态下不分配。
    pub fn process(&mut self, x: &[f64], out: &mut [f64]) {
        assert_eq!(
            out.len(),
            self.bands.len(),
            "输出缓冲长度必须等于带数"
        );

        if self.scratch.len() < x.len() {
            self.scratch.resize(x.len(), 0.0);
        }

        // 先取出长度，避免在循环里同时可变借用 self 的两个字段。
        let n = x.len();
        for (band, o) in self.bands.iter_mut().zip(out.iter_mut()) {
            let buf = &mut self.scratch[..n];
            buf.copy_from_slice(x);
            band.cascade.process_in_place(buf);
            *o = mean_square(buf) * band.power_gain;
        }
    }

    /// 各带均方值 → dB（相对给定参考）。
    ///
    /// `reference` 是参考声压：传 `noise_core::P0` 得 dB SPL，
    /// 传 `1.0` 得 dBFS。
    pub fn process_db(&mut self, x: &[f64], out_db: &mut [f64], reference: f64) {
        let n = self.bands.len();
        assert_eq!(out_db.len(), n, "输出缓冲长度必须等于带数");
        if self.ms_scratch.len() != n {
            self.ms_scratch.resize(n, 0.0);
        }

        // 拆开借用：process 需要 &mut self，而输出走 ms_scratch。
        let mut ms = std::mem::take(&mut self.ms_scratch);
        self.process(x, &mut ms);
        self.ms_scratch = ms;

        for (o, m) in out_db.iter_mut().zip(self.ms_scratch.iter()) {
            *o = mean_square_to_db(*m, reference);
        }
    }

    pub fn reset(&mut self) {
        for band in &mut self.bands {
            band.cascade.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen;

    const FS: f64 = 48_000.0;

    fn band_db(bank: &mut OctaveBank, x: &[f64]) -> Vec<f64> {
        let mut db = vec![0.0; bank.num_bands()];
        bank.process_db(x, &mut db, 1.0);
        db
    }

    #[test]
    fn tone_appears_in_its_own_band() {
        let mut bank = OctaveBank::with_default_bands(FS);
        let x = testgen::sine(63.0, FS, 48_000, 0.5);
        let db = band_db(&mut bank, &x);

        let peak_idx = db
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        assert_eq!(
            peak_idx, 1,
            "63 Hz 正弦的能量应落在 63 Hz 带，实测各带 dB = {db:?}"
        );
    }

    /// 这条是 Phase 1 低频自检的核心：1 kHz 的能量**不能**串进低频带，
    /// 否则用户播放 1 kHz 时看到低频带也抬起来，会误判麦克风低频正常。
    #[test]
    fn high_frequency_tone_does_not_leak_into_low_bands() {
        let mut bank = OctaveBank::with_default_bands(FS);
        let x = testgen::sine(1000.0, FS, 48_000, 0.5);
        let db = band_db(&mut bank, &x);

        let own = db[5]; // 1 kHz 带
        assert!(
            own - db[0] > 40.0,
            "1 kHz 与 31.5 Hz 带的隔离度应 > 40 dB，实测 {:.1} dB（各带 {db:?}）",
            own - db[0]
        );
        assert!(
            own - db[1] > 30.0,
            "1 kHz 与 63 Hz 带的隔离度应 > 30 dB，实测 {:.1} dB",
            own - db[1]
        );
    }

    #[test]
    fn low_frequency_tone_does_not_leak_into_high_bands() {
        let mut bank = OctaveBank::with_default_bands(FS);
        let x = testgen::sine(31.5, FS, 48_000, 0.5);
        let db = band_db(&mut bank, &x);

        assert!(
            db[0] - db[5] > 40.0,
            "31.5 Hz 与 1 kHz 带隔离度应 > 40 dB，实测 {:.1}（各带 {db:?}）",
            db[0] - db[5]
        );
    }

    /// 白噪声的各带读数**不该**相等——每个倍频程的带宽正比于中心频率，
    /// 所以白噪声下每带能量按 +3 dB/倍频程递增。
    ///
    /// 31.5 Hz 到 1 kHz 是 5 个倍频程，理论跨度 = 10·log₁₀(1000/31.5) ≈ 15 dB。
    /// 这条测试因此断言的是「跨度符合 +3 dB/oct」，而不是「大致相等」——
    /// 后者是个错误的期望，会让归一化真出错时反而测不出来。
    #[test]
    fn white_noise_bands_rise_three_db_per_octave() {
        let mut bank = OctaveBank::with_default_bands(FS);
        let x = testgen::white_noise(48_000, 0.3, 12345);
        let db = band_db(&mut bank, &x);

        let expected = 10.0 * (DEFAULT_CENTERS_HZ[5] / DEFAULT_CENTERS_HZ[0]).log10();
        let span = db[5] - db[0];
        assert!(
            (span - expected).abs() < 3.0,
            "白噪声跨度应约 {expected:.1} dB，实测 {span:.1} dB（各带 {db:?}）"
        );

        for w in db.windows(2) {
            assert!(
                w[1] > w[0],
                "白噪声下各带应单调递增，实测 {db:?}"
            );
        }
    }

    #[test]
    fn silence_produces_no_energy() {
        let mut bank = OctaveBank::with_default_bands(FS);
        let x = vec![0.0; 4800];
        let db = band_db(&mut bank, &x);
        for (i, d) in db.iter().enumerate() {
            assert!(d.is_finite(), "带 {i} 在静音下产生了 {d}");
            assert!(*d < -200.0, "带 {i} 在静音下应为极低值，实得 {d}");
        }
    }

    /// 分块处理必须与整块处理给出相同结果——否则检测器的时基
    /// 会随音频回调的缓冲大小漂移。
    #[test]
    fn chunked_processing_matches_whole_block() {
        let x = testgen::white_noise(48_000, 0.3, 999);

        let mut whole = OctaveBank::with_default_bands(FS);
        let a = band_db(&mut whole, &x);

        let mut chunked = OctaveBank::with_default_bands(FS);
        let mut ms = vec![0.0; chunked.num_bands()];
        let mut acc = vec![0.0; chunked.num_bands()];
        let mut count = 0usize;
        for c in x.chunks(480) {
            chunked.process(c, &mut ms);
            for (a, m) in acc.iter_mut().zip(ms.iter()) {
                *a += m * c.len() as f64;
            }
            count += c.len();
        }
        let b: Vec<f64> = acc
            .iter()
            .map(|s| 10.0 * (s / count as f64).log10())
            .collect();

        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            // 滤波器状态在块边界处是连续的，但能量累加的窗口划分不同，
            // 允许很小的差异。
            assert!(
                (x - y).abs() < 0.01,
                "带 {i} 分块与整块结果不一致：{x:.6} vs {y:.6}"
            );
        }
    }

    #[test]
    fn reset_clears_filter_state() {
        let mut bank = OctaveBank::with_default_bands(FS);
        let loud = testgen::sine(63.0, FS, 4800, 1.0);
        let _ = band_db(&mut bank, &loud);

        bank.reset();
        let silence = vec![0.0; 4800];
        let db = band_db(&mut bank, &silence);
        for (i, d) in db.iter().enumerate() {
            assert!(*d < -200.0, "reset 后带 {i} 仍残留能量：{d}");
        }
    }

    #[test]
    fn custom_band_set_is_respected() {
        let bank = OctaveBank::new(FS, &[63.0, 125.0, 250.0]);
        assert_eq!(bank.num_bands(), 3);
    }

    #[test]
    fn default_bands_are_not_degraded_at_supported_rates() {
        for fs in [16_000.0, 44_100.0, 48_000.0] {
            let bank = OctaveBank::with_default_bands(fs);
            assert!(!bank.is_degraded(), "{fs} Hz 下不应降级");
        }
    }
}
