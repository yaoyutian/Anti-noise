//! 全采样率稳定性扫描。
//!
//! # 这个文件防的是什么
//!
//! 本项目的所有滤波器系数都是**运行时按设备实际采样率现算**的（不写死
//! 48 kHz、也不加重采样器）。这带来一个只在换设备时才暴露的失效模式：
//! 某些设计频率在新采样率下越过 `0.45·fs`，双线性映射把极点折到接近
//! Nyquist，滤波器**要么失稳、要么响应完全变形**。
//!
//! 单元测试只覆盖了 48 kHz。这里把整条链拉到 8 kHz–192 kHz 全扫一遍，
//! 每个都做时域仿真——纯极点判据看不出 DF2T 在接近单位圆时的数值问题，
//! 喂信号看它炸不炸才是真的。

use noise_dsp::design::{butterworth_highpass, butterworth_lowpass};
use noise_dsp::octave::DEFAULT_CENTERS_HZ;
use noise_dsp::weighting::Weighting;
use noise_dsp::{AWeighting, CWeighting, Cascade, OctaveBank, ZWeighting};

/// 覆盖从蓝牙 HFP（8 kHz）到专业接口（192 kHz）的全部采样率。
///
/// 8 kHz 这一档不是假想：macOS 连上蓝牙耳机后会切到 HFP 模式，
/// 实际输入采样率就掉到 8/16 kHz。
const RATES: [f64; 8] = [
    8_000.0, 16_000.0, 22_050.0, 32_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0,
];

/// 确定性伪随机序列。自带实现而不是引入 `rand`：本 crate 的测试不该
/// 因为上游改了默认算法就在某天突然换个信号。
fn white_noise(n: usize) -> Vec<f64> {
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    (0..n)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
        })
        .collect()
}

/// 处理一段单位幅度白噪声，返回（输出是否全为有限值，输出的最大绝对值）。
///
/// 失稳的滤波器会指数发散，几千个样本内就溢出成 `inf`。
fn probe(mut process: impl FnMut(&mut [f64]), n: usize) -> (bool, f64) {
    let mut x = white_noise(n);
    process(&mut x);
    let all_finite = x.iter().all(|v| v.is_finite());
    let peak = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    (all_finite, peak)
}

/// 检测链：20–200 Hz 带通。触发路径用的就是这个。
fn detector_chain(fs: f64) -> Cascade {
    let mut sections = butterworth_highpass(fs, 20.0, 4).sections;
    sections.extend(butterworth_lowpass(fs, 200.0, 4).sections);
    Cascade::new(sections)
}

#[test]
fn detector_chain_is_stable_at_every_rate() {
    for fs in RATES {
        let mut c = detector_chain(fs);
        assert!(c.is_stable(), "{fs} Hz：极点判据报告不稳定");

        let (finite, peak) = probe(|x| c.process_in_place(x), 48_000);
        assert!(finite, "{fs} Hz：时域仿真出现非有限值（滤波器发散）");
        // 单位幅度白噪声过带通，峰值不该超过几倍。超过 100 说明有共振。
        assert!(
            peak < 100.0,
            "{fs} Hz：输出峰值 {peak:.1} 远超合理范围，怀疑接近单位圆的高 Q 极点"
        );
    }
}

#[test]
fn a_weighting_is_stable_at_every_rate() {
    for fs in RATES {
        let mut w = AWeighting::new(fs);
        assert!(w.is_stable(), "{fs} Hz：A 计权极点判据不稳定");

        let (finite, _) = probe(|x| w.process_in_place(x), 48_000);
        assert!(finite, "{fs} Hz：A 计权时域仿真发散");
    }
}

#[test]
fn c_weighting_is_stable_at_every_rate() {
    for fs in RATES {
        let mut w = CWeighting::new(fs);
        assert!(w.is_stable(), "{fs} Hz：C 计权极点判据不稳定");
        let (finite, _) = probe(|x| w.process_in_place(x), 48_000);
        assert!(finite, "{fs} Hz：C 计权时域仿真发散");
    }
}

#[test]
fn z_weighting_is_stable_and_never_degraded_everywhere() {
    for fs in RATES {
        let w = ZWeighting;
        assert!(w.is_stable());
        assert!(!w.is_degraded(), "{fs} Hz：Z 计权是无极点的直通，不该降级");
    }
}

/// A 计权的最高极点 12194 Hz 在 22.05 kHz 及以下会越过 `0.45·fs`。
///
/// **滤波器在这些采样率下仍然稳定**——所以只查稳定性会漏掉这个失效模式：
/// 双线性映射把越界的极点折到接近 Nyquist 的位置，滤波器不炸，但它的
/// 响应已经完全不是 A 计权曲线了。这正是 `degraded` 标记存在的理由。
#[test]
fn a_weighting_flags_degraded_below_32khz() {
    for fs in [8_000.0, 16_000.0, 22_050.0] {
        assert!(
            AWeighting::new(fs).is_degraded(),
            "{fs} Hz：A 计权必须置 degraded —— 12194 Hz 极点越过 0.45×{fs}"
        );
    }
    for fs in [32_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
        assert!(!AWeighting::new(fs).is_degraded(), "{fs} Hz：A 计权不应降级");
    }
}

/// IEC 61672-1 的 A 计权标称值。**规范值**，不是从公式算出来的。
const IEC_A_TABLE: &[(f64, f64)] = &[
    (20.0, -50.5),
    (31.5, -39.4),
    (63.0, -26.2),
    (125.0, -16.1),
    (250.0, -8.6),
    (500.0, -3.2),
    (1_000.0, 0.0),
    (2_000.0, 1.2),
    (4_000.0, 1.0),
    (8_000.0, -1.1),
];

/// **取证频段（20 Hz–2 kHz）在全部支持采样率下都准确到 ±0.5 dB。**
///
/// 这是实测扫描的结果：8 kHz 采样率下最差 0.28 dB，其余采样率都不超过
/// 0.13 dB。含义很实际——本项目报告的入户噪声能量全部落在 20–200 Hz，
/// 所以**即使在蓝牙 HFP 掉到 8 kHz 采样率时，A 计权的取证数值仍然可信**。
///
/// 这条测试的存在是为了挡住「低采样率下 A 计权不可用，所以干脆不报告」
/// 这种过度保守的处理：数据不支持那个结论。
#[test]
fn a_weighting_is_accurate_in_the_evidence_band_at_every_rate() {
    for fs in RATES {
        let w = AWeighting::new(fs);
        for &(f, expected) in IEC_A_TABLE {
            if f > 2_000.0 || f >= fs / 2.0 {
                continue;
            }
            let got = w.magnitude_db_at(f, fs);
            assert!(
                (got - expected).abs() < 0.5,
                "{fs} Hz：A 计权 @ {f} Hz 期望 {expected} dB，实得 {got:.3} dB"
            );
        }
    }
}

/// 偏差只出现在 2 kHz 以上，且随采样率降低而急剧增大。
///
/// 实测 8 kHz 处的偏差：
///
/// ```text
/// fs = 22 050 : +5.12 dB
/// fs = 32 000 : +1.53 dB
/// fs = 48 000 : +0.59 dB
/// ```
///
/// 这就是 `degraded` 标记真正在警告的东西——**不是「低频不准了」，
/// 而是「高频段被双线性畸变压坏了」**。22.05 kHz 下 5 dB 的误差意味着：
/// 若声源含显著的高频能量（说话、电视），A 计权**总级**会被低估；
/// 而八度带 1 kHz 及以下的读数不受影响。
#[test]
fn a_weighting_deviation_grows_above_2khz_at_low_rates() {
    let dev = |fs: f64, f: f64| (AWeighting::new(fs).magnitude_db_at(f, fs) - (-1.1)).abs();

    assert!(
        dev(22_050.0, 8_000.0) > 3.0,
        "22.05 kHz 下 8 kHz 处应有显著偏差，实得 {:.2} dB",
        dev(22_050.0, 8_000.0)
    );
    assert!(
        dev(48_000.0, 8_000.0) < 1.0,
        "48 kHz 下 8 kHz 处偏差应很小，实得 {:.2} dB",
        dev(48_000.0, 8_000.0)
    );

    // 偏差随采样率单调下降
    let d22 = dev(22_050.0, 8_000.0);
    let d32 = dev(32_000.0, 8_000.0);
    let d48 = dev(48_000.0, 8_000.0);
    assert!(
        d22 > d32 && d32 > d48,
        "高频偏差应随采样率升高而下降，实测 22k={d22:.2} 32k={d32:.2} 48k={d48:.2}"
    );
}

#[test]
fn octave_bank_is_clean_at_every_rate() {
    for fs in RATES {
        let mut bank = OctaveBank::with_default_bands(fs);

        // 8 kHz 下最高带（1 kHz，上拐 1414 Hz）仍远低于 0.45×8000=3600，
        // 所以默认带组在所有扫描采样率上都不该降级。
        assert!(!bank.is_degraded(), "{fs} Hz：默认八度带不应降级");

        // 用 200 Hz 正弦探针（各采样率下都远低于 Nyquist）
        let x: Vec<f64> = (0..48_000)
            .map(|i| 0.5 * (std::f64::consts::TAU * 200.0 * i as f64 / fs).sin())
            .collect();
        let mut ms = vec![0.0; bank.num_bands()];
        bank.process(&x, &mut ms);

        for (i, m) in ms.iter().enumerate() {
            assert!(
                m.is_finite() && *m >= 0.0,
                "{fs} Hz 带 {i}（{} Hz）输出无效：{m}",
                DEFAULT_CENTERS_HZ[i]
            );
        }
    }
}

/// 各带必须强烈抑制带外能量。
///
/// 这条是 **Phase 1「低频自检」成立的前提**：如果高频能量会串进低频带，
/// 那么手机播 1 kHz 时低频带也会抬起来，用户就会误判自己的麦克风低频正常，
/// 而整个项目的 go/no-go 判断就建立在那个信号上。
#[test]
fn octave_bands_reject_out_of_band_energy_at_every_rate() {
    for fs in RATES {
        // 8 kHz 采样率下 1 kHz 已接近可用上限，改用 500 Hz 探针。
        let probe_hz = if fs < 16_000.0 { 500.0 } else { 1_000.0 };

        let mut bank = OctaveBank::with_default_bands(fs);
        let x: Vec<f64> = (0..48_000)
            .map(|i| 0.5 * (std::f64::consts::TAU * probe_hz * i as f64 / fs).sin())
            .collect();

        let mut db = vec![0.0; bank.num_bands()];
        bank.process_db(&x, &mut db, 1.0);

        let low = db[0];
        let peak = db.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        assert!(
            peak - low > 30.0,
            "{fs} Hz：{probe_hz} Hz 探针在 31.5 Hz 带的隔离度只有 {:.1} dB（各带 {db:?}）",
            peak - low
        );
    }
}
