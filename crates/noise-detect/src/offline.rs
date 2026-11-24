//! 离线特征提取：把一段音频变成 [`FrameFeatures`] 流。
//!
//! # 为什么它是一等公民而不是测试辅助
//!
//! 在音频硬件就绪之前，这是唯一能回答「检测器在**真实信号**上表现如何」
//! 的东西。没有它，场景测试只能喂手搓的 dB 数字——那测的是检测器对
//! **我以为的**信号的反应，而不是对实际信号的反应。两者在低频上差别
//! 极大：63 Hz 的周期是 16 ms 而帧长只有 10 ms，任何关于「这一帧里有多少
//! 能量」的直觉都不可靠（`docs/detector-tuning.md` 第一节就是被这件事
//! 打脸的记录）。
//!
//! 它同时是 Phase 8 replay 工具的核心：`examples/replay.rs` 读 WAV、
//! 调 [`extract`]、打印 action 流。**同一条提取路径**，所以用录音调出来的
//! 参数在实机上成立——用两条路径的话，调出来的参数是在拟合另一套代码。
//!
//! # 处理顺序必须与实时路径一致
//!
//! ```text
//! 原始样本 ─┬─→ 触发带（20–200 Hz，未计权）→ 逐帧均方值 → trigger_mean_square
//!           ├─→ 不滤波，逐帧均方值 + 峰值      → broadband / crest
//!           └─→ 八度带组，逐帧均方值           → bands_db
//! ```
//!
//! **触发带滤波后的信号只用于检测，绝不写进录音**——录音永远是原始信号
//! （见 `docs/acoustics-notes.md`）。这里 `x` 全程只读。
//!
//! 实时路径分块喂、这里整段喂，但八度带和触发带的**滤波器状态都是连续
//! 跨帧的**，所以两者逐帧结果一致。

use crate::config::DetectorConfig;
use crate::frame::FrameAssembler;
use noise_dsp::cascade::Cascade;
use noise_dsp::design::{butterworth_highpass, butterworth_lowpass};
use noise_dsp::features::{mean_square_to_db_floored, spectral_flatness_db, FrameFeatures};
use noise_dsp::metrics::{crest_factor_db, mean_square};
use noise_dsp::octave::OctaveBank;
use noise_dsp::Coefficients;

/// 触发带的低端截止，Hz。
pub const TRIGGER_LOW_HZ: f64 = 20.0;

/// 触发带的高端截止，Hz。
///
/// 200 Hz 而不是更高：再往上就进入人声和大部分家电噪声的频段，
/// 而结构传声的能量集中在 20–200 Hz。
pub const TRIGGER_HIGH_HZ: f64 = 200.0;

/// 触发带每侧滤波器的阶数。
///
/// 4 阶给出 24 dB/oct 的裙边，足以把 500 Hz 以上的语音能量压到
/// 不影响判据的程度，同时把 20 Hz 以下的风扇/交通隆隆声挡在外面。
pub const TRIGGER_ORDER: usize = 4;

/// 20–200 Hz 触发带的系数，以及在设计频率被钳位时置起的 `degraded`。
///
/// 返回系数而不是 [`Cascade`]，是因为调用方可能只想要频响（画图、
/// 自检），不一定想跑信号。
pub fn trigger_band_sections(fs: f64) -> (Vec<Coefficients<f64>>, bool) {
    let hp = butterworth_highpass(fs, TRIGGER_LOW_HZ, TRIGGER_ORDER);
    let lp = butterworth_lowpass(fs, TRIGGER_HIGH_HZ, TRIGGER_ORDER);
    let degraded = hp.degraded || lp.degraded;
    let mut sections = hp.sections;
    sections.extend(lp.sections);
    (sections, degraded)
}

/// 触发带级联滤波器。
pub fn build_trigger_band(fs: f64) -> Cascade {
    Cascade::new(trigger_band_sections(fs).0)
}

/// 离线提取的全部产物，含调用方需要的旁路信息。
#[derive(Clone, Debug)]
pub struct Extracted {
    /// 逐帧特征，按时间顺序。
    pub features: Vec<FrameFeatures>,
    /// 触发带或八度带的设计频率被钳位（采样率过低）。
    ///
    /// 一路传播到 session manifest 和 UI 警告。**静默降级比降级本身更糟**：
    /// 用户以为自己在看 20–200 Hz 的证据，实际上看的是被削过的东西。
    pub degraded: bool,
    /// 实际使用的帧长（毫秒）。`frame_len` 是整数个样本，除不尽时会圆整，
    /// 这个值是**真实时基**——不是请求值。
    pub frame_ms: f64,
    /// 每帧的样本数。
    pub frame_len: usize,
    /// 因不满一帧而丢弃的尾部样本数。
    pub tail_samples: usize,
}

/// 把一段音频转成特征流。
///
/// * `x` —— 单声道、设备原生采样率、**原始未滤波**的样本
/// * `fs` —— **实际达成**的采样率（不是请求的）
/// * `cfg` —— 只用其中的 `frame_ms`
/// * `reference` —— 转 dB 用的参考。`1.0` 得 dBFS；传 `noise_core::P0`
///   得 dB SPL——**但只有在做完校准之后那个绝对值才有意义**
///
/// 触发带和八度带都在这里按 `fs` 现场设计。设备实际给到 44.1 kHz 或
/// 96 kHz 时，同一段音频的判据不变。
pub fn extract(x: &[f64], fs: f64, cfg: &DetectorConfig, reference: f64) -> Extracted {
    let (sections, trigger_degraded) = trigger_band_sections(fs);
    let mut trigger = Cascade::new(sections);
    let mut octave = OctaveBank::with_default_bands(fs);
    let n_bands = octave.num_bands();

    // 触发带整段过一遍。滤波器状态在这里是连续的，与实时路径逐块推进
    // 等价（IIR 是可结合的）。
    let mut filtered = x.to_vec();
    trigger.process_in_place(&mut filtered);

    // 用装配器算帧长，保证与实时路径的帧边界**逐样本一致**。
    let assembler = FrameAssembler::new(fs, cfg.frame_ms);
    let frame_len = assembler.frame_len();
    let frame_ms = assembler.frame_ms();

    let total_frames = filtered.len() / frame_len;
    let tail_samples = filtered.len() - total_frames * frame_len;

    let mut out = Vec::with_capacity(total_frames);
    let mut band_db = vec![0.0f64; n_bands];

    for k in 0..total_frames {
        let lo = k * frame_len;
        let hi = lo + frame_len;
        let raw = &x[lo..hi];
        let filt = &filtered[lo..hi];

        // 八度带吃**原始**信号（滤波只服务于触发判据）。
        octave.process_db(raw, &mut band_db, reference);

        let peak = raw.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let broad_ms = mean_square(raw);

        out.push(FrameFeatures {
            frame_index: k as u64,
            t_ms: k as f64 * frame_ms,
            trigger_mean_square: mean_square(filt),
            broadband_mean_square: broad_ms,
            crest_db: crest_factor_db(peak, broad_ms),
            flatness: spectral_flatness_db(&band_db),
            bands_db: band_db.clone(),
            reference,
        });
    }

    Extracted {
        features: out,
        degraded: trigger_degraded || octave.is_degraded(),
        frame_ms,
        frame_len,
        tail_samples,
    }
}

/// 便捷：均方值 → dB，与特征侧同一个转换。
pub fn ms_to_db(mean_square: f64, reference: f64) -> f64 {
    mean_square_to_db_floored(mean_square, reference)
}

#[cfg(test)]
mod tests {
    use super::*;
    use noise_dsp::testgen;

    const FS: f64 = 48_000.0;

    fn cfg() -> DetectorConfig {
        DetectorConfig::default()
    }

    /// 帧长与帧数必须与装配器完全一致，否则实时路径与离线路径
    /// 会在事件时间戳上出现系统性偏移。
    #[test]
    fn framing_matches_the_assembler() {
        let x = testgen::white_noise(FS as usize, 0.3, 7);
        let e = extract(&x, FS, &cfg(), 1.0);

        let asm = FrameAssembler::new(FS, cfg().frame_ms);
        assert_eq!(e.frame_len, asm.frame_len());
        assert_eq!(e.frame_ms, asm.frame_ms());
        assert_eq!(e.tail_samples, x.len() % asm.frame_len());
        assert_eq!(e.features.len(), x.len() / asm.frame_len());
    }

    #[test]
    fn timestamps_advance_by_the_real_frame_length() {
        let x = testgen::white_noise(48_000, 0.3, 11);
        let e = extract(&x, FS, &cfg(), 1.0);
        for (k, f) in e.features.iter().enumerate() {
            assert!((f.t_ms - k as f64 * e.frame_ms).abs() < 1e-9);
            assert_eq!(f.frame_index, k as u64);
        }
    }

    /// **触发带必须只让低频通过。** 这是整个检测器的入口，
    /// 它漏了高频的话，说话和音乐都会变成「脚步声」。
    #[test]
    fn trigger_band_rejects_high_frequencies() {
        let mut c = build_trigger_band(FS);
        let mut x = testgen::sine(2000.0, FS, 48_000, 0.5);
        let before = mean_square(&x);
        c.process_in_place(&mut x);
        let after = mean_square(&x);
        let attenuation_db = 10.0 * (before / after).log10();
        assert!(
            attenuation_db > 40.0,
            "2 kHz 应被触发带衰减 40 dB 以上，实测 {attenuation_db:.1} dB"
        );
    }

    #[test]
    fn trigger_band_passes_the_target_band() {
        for freq in [31.5, 63.0, 125.0] {
            let mut c = build_trigger_band(FS);
            let mut x = testgen::sine(freq, FS, 48_000, 0.5);
            let before = mean_square(&x);
            c.process_in_place(&mut x);
            let after = mean_square(&x);
            let gain_db = 10.0 * (after / before).log10();
            assert!(
                gain_db.abs() < 3.0,
                "{freq} Hz 应基本无衰减地通过触发带，实测 {gain_db:+.2} dB"
            );
        }
    }

    /// 稳态 63 Hz 正弦的帧均方值起伏的**解析值**。
    ///
    /// 帧长 N 个样本、正弦频率 f（θ = 2πf/fs）时，帧均方值随起始相位在
    /// `(A²/2)(1 ∓ r)` 之间摆动，其中
    ///
    /// ```text
    /// r = |sin(θN)| / (N·|sin θ|)
    /// ```
    ///
    /// 48 kHz / 480 样本 / 63 Hz 下 θN ≈ 3.959 rad，r ≈ 0.184，
    /// 于是跨度 ≈ 1.6 dB。
    fn steady_state_span_db(f: f64, fs: f64, n: usize) -> f64 {
        let theta = std::f64::consts::TAU * f / fs;
        let r = (theta * n as f64).sin().abs() / (n as f64 * theta.sin().abs());
        10.0 * ((1.0 + r) / (1.0 - r)).log10()
    }

    /// 10 秒 63 Hz 正弦——足够看清瞬态，也足够长到稳态部分自成一段。
    fn long_tone() -> Vec<f64> {
        testgen::sine(63.0, FS, (FS * 10.0) as usize, 0.5)
    }

    /// 低频正弦的帧间起伏不是 bug，是 10 ms 帧装不下 63 Hz 的整数个周期
    /// 的必然结果。这条测试把它的**解析量级**钉下来——因为「起伏多大算
    /// 正常」正是 `min_rise_db` 的取值依据，凭感觉定会把稳态噪声也触发。
    #[test]
    fn steady_state_fluctuation_matches_the_closed_form() {
        let f = 63.0;
        let e = extract(&long_tone(), FS, &cfg(), 1.0);

        // 跳过起始瞬态（见下一条测试），只看稳态。
        let steady: Vec<f64> = e.features[100..].iter().map(|f| f.trigger_db()).collect();
        let lo = steady.iter().cloned().fold(f64::INFINITY, f64::min);
        let hi = steady.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let span = hi - lo;

        let expected = steady_state_span_db(f, FS, e.frame_len);
        assert!(
            (span - expected).abs() < 0.5,
            "稳态起伏应约 {expected:.2} dB（解析值），实测 {span:.2} dB"
        );
    }

    /// **第 0 帧的电平是错的——低了约 7 dB。**
    ///
    /// 滤波器从全零状态起步，而 63 Hz 的周期是 16 ms、帧长只有 10 ms：
    /// 头 480 个样本里信号还没走完一个周期，高通段仍在从零爬起来，
    /// 于是这一帧的能量被严重低估。第 1 帧就恢复正常。
    ///
    /// 实测（48 kHz，63 Hz 正弦，幅度 0.5）：
    ///
    /// ```text
    /// 帧 0   -14.97 dB   ← 被低估 6.9 dB
    /// 帧 1    -8.11 dB   ← 恢复正常
    /// 帧 100+ -9.61 dB   ← 稳态中心
    /// ```
    ///
    /// 后果不是「看起来难看」，而是两个具体的错误：
    ///
    /// 1. 底噪若用第 0 帧播种，会**低 7 dB**，系统在头几秒过度敏感；
    /// 2. 第 0 帧 → 第 1 帧构成一个 **+6.9 dB 的假上升沿**。
    ///
    /// 所以 `sm::Detector` 显式跳过第 0 帧的底噪播种（见
    /// `sm.rs::first_frame_is_not_used_to_seed_the_floor`）。这条测试
    /// 钉住这个现象本身，免得哪天有人「优化」掉那个跳过。
    #[test]
    fn first_frame_is_depressed_by_the_filters_zero_initial_state() {
        let e = extract(&long_tone(), FS, &cfg(), 1.0);

        let steady_min = e.features[100..]
            .iter()
            .map(|f| f.trigger_db())
            .fold(f64::INFINITY, f64::min);

        let first = e.features[0].trigger_db();
        assert!(
            steady_min - first > 3.0,
            "第 0 帧应显著低于稳态（实测第 0 帧 {first:.2} dB，\
             稳态最低 {steady_min:.2} dB）"
        );

        // 从第 1 帧起就必须正常——如果这条不成立，说明不只是「第一帧」
        // 的问题，而是整个起始段都不可信，处理方式要重新设计。
        let from_second = e.features[1..100]
            .iter()
            .map(|f| f.trigger_db())
            .fold(f64::INFINITY, f64::min);
        assert!(
            from_second - steady_min > -1.0,
            "第 1 帧起就该回到稳态区间（实测最低 {from_second:.2} dB，\
             稳态最低 {steady_min:.2} dB）"
        );
    }

    #[test]
    fn silence_extracts_finite_values() {
        let x = vec![0.0; 48_000];
        let e = extract(&x, FS, &cfg(), 1.0);
        assert!(!e.features.is_empty());
        for f in &e.features {
            assert!(f.trigger_db().is_finite(), "静音产生了非有限 dB");
            assert!(f.flatness.is_finite());
            for b in &f.bands_db {
                assert!(b.is_finite(), "静音下带电平非有限：{b}");
            }
        }
    }

    #[test]
    fn band_levels_track_a_63hz_tone() {
        let x = testgen::sine(63.0, FS, 48_000, 0.5);
        let e = extract(&x, FS, &cfg(), 1.0);
        let mid = &e.features[e.features.len() / 2];
        let lf = mid.band_at_hz(63.0).unwrap();
        let hf = mid.band_at_hz(1000.0).unwrap();
        assert!(
            lf - hf > 40.0,
            "63 Hz 音在 63 Hz 带应远高于 1 kHz 带，实测 {:.1} dB",
            lf - hf
        );
        assert!(mid.lf_ratio_db() > 30.0, "实得 {}", mid.lf_ratio_db());
    }

    /// 采样率变了，判据不该变。44.1 kHz 下同一段信号应给出
    /// 与 48 kHz 实质相同的触发带电平。
    #[test]
    fn results_are_consistent_across_sample_rates() {
        let mut levels = Vec::new();
        for fs in [44_100.0, 48_000.0, 96_000.0] {
            let x = testgen::sine(63.0, fs, (fs * 1.0) as usize, 0.5);
            let e = extract(&x, fs, &cfg(), 1.0);
            let n = e.features.len();
            let mid: Vec<f64> = e.features[n / 4..3 * n / 4]
                .iter()
                .map(|f| f.trigger_db())
                .collect();
            let mean = mid.iter().sum::<f64>() / mid.len() as f64;
            levels.push((fs, mean));
        }
        let reference = levels[0].1;
        for (fs, lvl) in &levels {
            assert!(
                (lvl - reference).abs() < 1.0,
                "{fs} Hz 下触发带电平 {lvl:.2} dB 与 44.1 kHz 的 {reference:.2} dB 相差过大"
            );
        }
    }
}
