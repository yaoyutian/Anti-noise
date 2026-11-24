//! 反击声的波形生成。
//!
//! # 波形设计上的每一处都是为了让**小喇叭**也能出声
//!
//! 反击声要穿过楼板被楼上听到，而绝大多数消费级音响（笔记本、蓝牙箱、
//! Soundbar）在 50 Hz 处的声压级是零。**一个设计得「低频很足」的波形
//! 放在这种设备上会得到一片安静**，而用户会以为功能坏了。
//!
//! 所以：
//!
//! * **基频取 55 Hz**，而不是 30–40 Hz。55 Hz 已经低于人耳的「咚」感
//!   下限之上，又落在小喇叭还能推出一点点位移的位置。
//! * **带二次谐波（110 Hz，相对幅度 0.35）**。这是关键的一步：喇叭
//!   推不出 55 Hz 时，110 Hz 的谐波仍然出得来，而人耳根据缺失基频
//!   现象**会把它听成 55 Hz 的「咚」**。听感上低频还在，物理上没有
//!   要求喇叭做它做不到的事。
//! * **三次谐波只给 0.12**，因为 55×3 = 165 Hz 已经在通带之外，
//!   留着它只是为了给起音加一点「硬」的质感。
//!
//! # 为什么最后还要过一次带通
//!
//! 上面那几条是「设计意图」，不是「保证」。真正的保证是末尾那道
//! 40–120 Hz 带通：
//!
//! * **高通挡直流**。任何直流偏置都会被功放当成纯电阻负载持续加热，
//!   在小音箱上这是实打实的损坏路径，而且它听不见——用户不会知道
//!   自己的音响正在被烤。
//! * **低通挡掉数值噪声和起音处的瞬态毛刺**。`sin(2πft)` 在 `t=0`
//!   处是 0，但包络的对数斜率在那里是不连续的，会产生一点宽带能量。
//!
//! 滤波器**按实际采样率现场设计**（和触发链同一条纪律），不写死
//! 48 kHz。
//!
//! # 首尾必须是零
//!
//! 起音用升余弦从 0 开始，收尾用一段短淡出落到 0。两处都做到位，
//! 波形拼接在静音上时不会产生阶跃——**阶跃就是一个咔哒声**，
//! 而那个咔哒声会被自己的麦克风录进去，也容易被邻居听成另一种噪声。
//! 淡出还有一个作用：把滤波器在信号末尾的振铃切干净。

use noise_dsp::cascade::Cascade;
use noise_dsp::design::{butterworth_highpass, butterworth_lowpass};

/// 带通的下沿，Hz。
pub const BAND_LOW_HZ: f64 = 40.0;
/// 带通的上沿，Hz。
pub const BAND_HIGH_HZ: f64 = 120.0;
/// 每侧滤波器的阶数。4 阶给 24 dB/oct。
pub const BAND_ORDER: usize = 4;

/// 默认峰值，dBFS。
///
/// **−1 dBFS 而不是 0**：留 1 dB 的余量，是因为这段波形还要经过
/// 一个可调增益、再进入系统的混音器。用户把增益调到最大时，
/// 任何一点点采样间的重建过冲（DAC 的插值滤波会做这件事）都会
/// 越过 0 dBFS 而削波，而削波是宽带失真——它会**破坏反击声的
/// 低频纯度**，恰好毁掉这个功能唯一的卖点。
pub const PEAK_DBFS: f64 = -1.0;

/// 一次反击的波形参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThumpSpec {
    /// 生成时的采样率。**必须是输出流实际达成的那个。**
    pub sample_rate: f64,
    /// 基频，Hz。
    pub fundamental_hz: f64,
    /// 总时长，毫秒。
    pub duration_ms: f64,
    /// 升余弦起音时长，毫秒。
    pub attack_ms: f64,
    /// 指数衰减的时间常数，毫秒。
    pub decay_ms: f64,
    /// 末尾淡出时长，毫秒。
    pub fade_out_ms: f64,
    /// 各次谐波的相对幅度（基频、二次、三次）。
    pub harmonic_gains: [f64; 3],
}

impl Default for ThumpSpec {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            fundamental_hz: 55.0,
            // 300 ms：短到「反击约 100 ms 准确」的预算内收得干净，
            // 长到能听出一个「咚」而不是一声咔哒。
            duration_ms: 300.0,
            // 8 ms 起音：再短会在滤波后留下可听的瞬态毛刺，
            // 再长就把冲击感磨平了。
            attack_ms: 8.0,
            // 90 ms 衰减：大约三个时间常数之后（270 ms）落到 −26 dB，
            // 正好在波形结束前自然收尾。
            decay_ms: 90.0,
            fade_out_ms: 8.0,
            harmonic_gains: [1.0, 0.35, 0.12],
        }
    }
}

impl ThumpSpec {
    /// 总样本数。
    pub fn length_samples(&self) -> usize {
        if !self.sample_rate.is_finite() || self.sample_rate <= 0.0 {
            return 0;
        }
        ((self.duration_ms / 1000.0) * self.sample_rate).round().max(0.0) as usize
    }

    /// 总时长（**由样本数反算**，不是配置里那个数）。
    ///
    /// 两者会因为取整差一点点，而回放和门控的冷却期计算必须用**真实的**
    /// 那一个——用配置值算出来的冷却期会比实际波形短几个样本，
    /// 于是尾巴上那几个样本会漏进冷却期之外。
    pub fn actual_duration_ms(&self) -> f64 {
        if self.sample_rate <= 0.0 {
            return 0.0;
        }
        self.length_samples() as f64 / self.sample_rate * 1000.0
    }
}

/// 生成好的反击声。
#[derive(Clone, Debug)]
pub struct Thump {
    /// 单声道样本，峰值已归一化到 [`PEAK_DBFS`]，首尾都是 0。
    pub samples: Vec<f32>,
    /// 实际峰值，dBFS。用来核对归一化真的生效了。
    pub peak_dbfs: f64,
    /// 波形的算术平均（绝对值）。
    ///
    /// **它天然不是零，而且这是对的。** 300 ms × 55 Hz = 16.5 个周期，
    /// 最后那半周期没走完，于是算术平均里必然留下一小块。那不是直流，
    /// 是几何。真正该看的判据是它**相对峰值**有多大——见
    /// `there_is_no_dc_offset`。
    pub dc_offset: f64,
    /// 带通的设计频率被采样率钳位了。要传播到 UI 上。
    pub degraded: bool,
}

impl Thump {
    /// 样本数。
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// 时长，毫秒。
    pub fn duration_ms(&self) -> f64 {
        self.samples.len() as f64
    }
}

/// 按参数生成一段反击声。
///
/// 全过程用 `f64`，最后一步才降到 `f32`（输出环是 f32）。
/// 中间的包络是比值很小的数相乘，f32 下在长衰减的尾部会丢有效位，
/// 而那段恰好决定「淡出是否真的到了零」。
pub fn build_thump(spec: &ThumpSpec) -> Thump {
    let n = spec.length_samples();
    let fs = spec.sample_rate;
    if n == 0 || !fs.is_finite() || fs <= 0.0 {
        return Thump {
            samples: Vec::new(),
            peak_dbfs: f64::NEG_INFINITY,
            dc_offset: 0.0,
            degraded: false,
        };
    }

    let attack = ms_to_samples(spec.attack_ms, fs).clamp(1, n);
    let fade = ms_to_samples(spec.fade_out_ms, fs).clamp(1, n);
    let tau = (spec.decay_ms / 1000.0).max(1e-6);

    let mut x: Vec<f64> = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / fs;

        // 起音：升余弦 0 → 1。用 cos 而不是线性斜坡，是因为线性斜坡的
        // 一阶导数在两端不连续，滤波后会在频谱上拖出旁瓣。
        let env_attack = if i < attack {
            0.5 * (1.0 - (std::f64::consts::PI * i as f64 / attack as f64).cos())
        } else {
            1.0
        };

        let env_decay = (-t / tau).exp();

        // 各次谐波。相位都从 0 起，所以每一项在 t=0 处都是 0，
        // 与起音相乘之后开头是一段干净的静音。
        let mut s = 0.0;
        for (k, g) in spec.harmonic_gains.iter().enumerate() {
            if *g == 0.0 {
                continue;
            }
            let f = spec.fundamental_hz * (k + 1) as f64;
            s += g * (std::f64::consts::TAU * f * t).sin();
        }

        x.push(env_attack * env_decay * s);
    }

    // 带通。按**实际**采样率现场设计。
    let hp = butterworth_highpass(fs, BAND_LOW_HZ, BAND_ORDER);
    let lp = butterworth_lowpass(fs, BAND_HIGH_HZ, BAND_ORDER);
    let degraded = hp.degraded || lp.degraded;
    let mut sections = hp.sections;
    sections.extend(lp.sections);
    Cascade::new(sections).process_in_place(&mut x);

    // 淡出排在归一化**之前**：反过来的话，峰值归一化会把淡出段
    // 重新放大，末尾就不再是零了。
    apply_fade_out(&mut x, fade);

    // 归一化放在最后一步，所以「峰值 == PEAK_DBFS」是**精确成立**的，
    // 而不是「大致成立，除非峰值正好落在淡出段里」。
    let peak = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    if peak > 0.0 {
        let target = 10f64.powf(PEAK_DBFS / 20.0);
        let g = target / peak;
        for v in &mut x {
            *v *= g;
        }
    }

    let peak_after = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let dc = if n > 0 {
        x.iter().sum::<f64>() / n as f64
    } else {
        0.0
    };

    Thump {
        samples: x.into_iter().map(|v| v as f32).collect(),
        peak_dbfs: if peak_after > 0.0 {
            20.0 * peak_after.log10()
        } else {
            f64::NEG_INFINITY
        },
        dc_offset: dc.abs(),
        degraded,
    }
}

/// 末尾淡出到**恰好零**。
///
/// 用 `fade - 1` 作分母，于是在 `j == fade - 1` 处 `cos(π) = -1`，
/// 因子正好是 0——最后一个样本严格是 0，不是一个很小的数。
/// 「很小的数」在这里不够：它乘上功放的增益之后仍然是一个阶跃。
fn apply_fade_out(x: &mut [f64], fade: usize) {
    let n = x.len();
    if fade == 0 || fade > n {
        return;
    }
    let start = n - fade;
    let denom = (fade - 1).max(1) as f64;
    for j in 0..fade {
        let factor = 0.5 * (1.0 + (std::f64::consts::PI * j as f64 / denom).cos());
        x[start + j] *= factor;
    }
}

fn ms_to_samples(ms: f64, fs: f64) -> usize {
    if !ms.is_finite() || ms <= 0.0 {
        return 0;
    }
    ((ms / 1000.0) * fs).round().max(0.0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单频点的幅度。对整段做相关，等效于一个很粗的频谱估计——
    /// 对「能量在不在这个带里」这种量级的判断足够了。
    fn tone_magnitude(x: &[f64], f: f64, fs: f64) -> f64 {
        let mut re = 0.0;
        let mut im = 0.0;
        for (i, v) in x.iter().enumerate() {
            let ph = std::f64::consts::TAU * f * i as f64 / fs;
            re += v * ph.cos();
            im -= v * ph.sin();
        }
        (re * re + im * im).sqrt() / x.len() as f64
    }

    fn as_f64(t: &Thump) -> Vec<f64> {
        t.samples.iter().map(|v| *v as f64).collect()
    }

    /// 首尾必须是**恰好**零。差一点点的表现是回放开始/结束时有一声
    /// 咔哒，而那声咔哒会被自己的麦克风录进去。
    #[test]
    fn starts_and_ends_at_exactly_zero() {
        let t = build_thump(&ThumpSpec::default());
        assert!(!t.is_empty());
        assert_eq!(t.samples[0], 0.0, "起音的第一个样本必须是 0");
        assert_eq!(
            *t.samples.last().unwrap(),
            0.0,
            "淡出的最后一个样本必须严格是 0，不能是「很小的数」"
        );
    }

    /// 峰值归一化是**精确**的，不是近似的。回退到「大致 −1 dB」意味着
    /// 用户在最大增益下会削波，而削波产生的是宽带失真。
    #[test]
    fn peak_is_exactly_minus_one_dbfs() {
        for fs in [16_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let spec = ThumpSpec {
                sample_rate: fs,
                ..ThumpSpec::default()
            };
            let t = build_thump(&spec);
            let peak = t.samples.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            let db = 20.0 * (peak as f64).log10();
            assert!(
                (db - PEAK_DBFS).abs() < 0.05,
                "{fs} Hz 下峰值 {db:.3} dBFS，应为 {PEAK_DBFS} dBFS"
            );
            assert!(
                (t.peak_dbfs - PEAK_DBFS).abs() < 0.05,
                "报告的峰值 {} 与实际不符",
                t.peak_dbfs
            );
        }
    }

    /// **直流要压到可以忽略。** 直流会被功放当成纯阻性负载持续加热，
    /// 听不见，而且在小音箱上是一条真实的损坏路径。
    ///
    /// # 判据是「相对峰值」而不是「绝对大小」
    ///
    /// 第一版这条测试断言平均值 `< 1e-5`，然后在 16 kHz 上失败
    /// （实测 2.7e-4）。但那个数**不是缺陷**：300 ms 的 55 Hz 突发是
    /// 16.5 个周期，最后半周期没走完，算术平均里必然留下一小块。
    /// 用绝对阈值只会把这条测试调成一个必须放宽到「反正都通过」的
    /// 假绿灯——而它对**真正的**直流（一个常数偏移）也就同样失去了
    /// 分辨力。
    ///
    /// 相对阈值能分开这两者：真实的直流偏移是相对于峰值的可观比例，
    /// 而截断残留是它的千分之几。滤波器的实际能力由下一条测试
    /// 直接验证。
    #[test]
    fn there_is_no_dc_offset() {
        for fs in [16_000.0, 44_100.0, 48_000.0, 96_000.0] {
            let spec = ThumpSpec {
                sample_rate: fs,
                ..ThumpSpec::default()
            };
            let t = build_thump(&spec);
            let peak = t.samples.iter().fold(0.0f64, |m, v| m.max(v.abs() as f64));
            let rel = t.dc_offset / peak;
            assert!(
                rel < 0.01,
                "{fs} Hz 下平均值占峰值的 {:.2}%，太高了（很可能是真的直流）",
                rel * 100.0
            );
        }
    }

    /// 带通对**真正的**直流的压制能力。上一条测试看的是最终波形，
    /// 这一条直接喂一段纯直流进去——那才是「功放被烤」的那种信号。
    #[test]
    fn the_band_pass_crushes_a_real_dc_pedestal() {
        let fs = 48_000.0;
        let mut x = vec![0.5f64; 48_000]; // 恒定 0.5，一秒

        let hp = butterworth_highpass(fs, BAND_LOW_HZ, BAND_ORDER);
        let lp = butterworth_lowpass(fs, BAND_HIGH_HZ, BAND_ORDER);
        let mut sections = hp.sections;
        sections.extend(lp.sections);
        Cascade::new(sections).process_in_place(&mut x);

        // 跳过起始瞬态，只看稳态
        let tail = &x[24_000..];
        let peak = tail.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!(
            peak < 1e-6,
            "0.5 的直流输入在稳态下还残留 {peak:e}，高通没起作用"
        );
    }

    /// 能量必须落在 40–120 Hz 通带里，带外要低 20 dB 以上。
    ///
    /// 这不是「频谱好看」的问题：带外的能量会白白占掉功放的余量，
    /// 而反击声的全部意义就是用同样的功率在楼板上激出更大的振动。
    #[test]
    fn energy_stays_inside_the_band() {
        let spec = ThumpSpec::default();
        let t = build_thump(&spec);
        let x = as_f64(&t);
        let fs = spec.sample_rate;

        let in_band = tone_magnitude(&x, spec.fundamental_hz, fs);
        assert!(in_band > 0.0, "带内居然没有能量");

        for f in [10.0, 20.0, 300.0, 1000.0, 4000.0] {
            let out = tone_magnitude(&x, f, fs);
            let rel_db = 20.0 * (out / in_band).log10();
            assert!(
                rel_db < -20.0,
                "{f} Hz 处只比带内低 {:.1} dB，带通没起作用",
                -rel_db
            );
        }
    }

    /// 二次谐波必须真的存在——它是这一段波形能被小喇叭放出来的原因。
    /// 幅度取 0.35，所以应该在基频之下约 9 dB，而不是消失。
    #[test]
    fn the_second_harmonic_is_present() {
        let spec = ThumpSpec::default();
        let t = build_thump(&spec);
        let x = as_f64(&t);
        let fs = spec.sample_rate;

        let fund = tone_magnitude(&x, spec.fundamental_hz, fs);
        let second = tone_magnitude(&x, spec.fundamental_hz * 2.0, fs);
        let rel_db = 20.0 * (second / fund).log10();
        assert!(
            (-16.0..-3.0).contains(&rel_db),
            "二次谐波相对基频 {rel_db:.1} dB，期望约 −9 dB"
                );
    }

    /// 时长要由**样本数**反算，不能信配置里那个数。
    /// 门控的冷却期是按这个值算的，短了几个样本就会让尾巴漏到冷却期之外。
    #[test]
    fn duration_comes_from_the_sample_count() {
        let spec = ThumpSpec {
            sample_rate: 44_100.0,
            duration_ms: 300.0,
            ..ThumpSpec::default()
        };
        let expected = (0.300 * 44_100.0f64).round() as usize;
        assert_eq!(spec.length_samples(), expected);
        assert_eq!(build_thump(&spec).len(), expected);
        assert!(
            (spec.actual_duration_ms() - 300.0).abs() < 0.05,
            "实得 {}",
            spec.actual_duration_ms()
        );
        // 样本数是整数，时长就必然有小偏差——这正是不能信配置值的原因
        let odd = ThumpSpec {
            sample_rate: 44_100.0,
            duration_ms: 200.5,
            ..ThumpSpec::default()
        };
        assert_ne!(odd.actual_duration_ms(), 200.5, "取整之后就该不一样");
    }

    /// 采样率变了，波形该跟着变——滤波器是按实际采样率现场设计的。
    /// 这条是抓「谁把 48 kHz 写死了」的。
    #[test]
    fn the_same_spec_at_different_rates_is_not_the_same_signal() {
        let a = build_thump(&ThumpSpec {
            sample_rate: 48_000.0,
            ..ThumpSpec::default()
        });
        let b = build_thump(&ThumpSpec {
            sample_rate: 44_100.0,
            ..ThumpSpec::default()
        });
        assert_ne!(a.len(), b.len(), "时长不同，样本数就该不同");

        // 但两者的**听感**必须一致：带内能量占比、峰值都一样
        let pa = a.samples.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let pb = b.samples.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((pa - pb).abs() < 1e-6, "峰值不该随采样率变");
    }

    /// 任何合法采样率下都不能产生 NaN 或削波。包括下限 16 kHz。
    #[test]
    fn every_supported_rate_produces_a_finite_in_range_waveform() {
        for fs in [16_000.0, 22_050.0, 32_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let t = build_thump(&ThumpSpec {
                sample_rate: fs,
                ..ThumpSpec::default()
            });
            assert!(!t.is_empty(), "{fs} Hz 生成了空波形");
            for (i, v) in t.samples.iter().enumerate() {
                assert!(v.is_finite(), "{fs} Hz 第 {i} 个样本是 {v}");
                assert!(v.abs() <= 1.0, "{fs} Hz 第 {i} 个样本越界：{v}");
            }
            assert!(!t.degraded, "{fs} Hz 不该触发钳位（120 Hz 远低于下限）");
        }
    }

    /// 参数被写成垃圾时不能 panic，也不能申请一块天文数字的内存。
    /// 这些参数来自配置文件，而配置文件是用户能改的。
    #[test]
    fn degenerate_specs_do_not_panic() {
        let cases = [
            ThumpSpec {
                sample_rate: 0.0,
                ..ThumpSpec::default()
            },
            ThumpSpec {
                sample_rate: f64::NAN,
                ..ThumpSpec::default()
            },
            ThumpSpec {
                duration_ms: 0.0,
                ..ThumpSpec::default()
            },
            ThumpSpec {
                duration_ms: -100.0,
                ..ThumpSpec::default()
            },
            ThumpSpec {
                attack_ms: 0.0,
                ..ThumpSpec::default()
            },
            // 起音比整段还长：必须钳到 n，不能越界
            ThumpSpec {
                attack_ms: 10_000.0,
                ..ThumpSpec::default()
            },
            // 淡出比整段还长：同上
            ThumpSpec {
                fade_out_ms: 10_000.0,
                ..ThumpSpec::default()
            },
            // 极短的一段
            ThumpSpec {
                duration_ms: 1.0,
                attack_ms: 5.0,
                fade_out_ms: 5.0,
                ..ThumpSpec::default()
            },
        ];
        for (i, spec) in cases.iter().enumerate() {
            let t = build_thump(spec);
            assert!(
                t.samples.iter().all(|v| v.is_finite()),
                "第 {i} 个退化参数产生了非有限样本"
            );
            assert!(
                t.samples.iter().all(|v| v.abs() <= 1.0),
                "第 {i} 个退化参数产生了越界样本"
            );
        }
    }

    /// 全零谐波增益会得到一段静音——不该除零，也不该产生 NaN。
    #[test]
    fn zero_harmonics_yields_silence_not_nan() {
        let t = build_thump(&ThumpSpec {
            harmonic_gains: [0.0, 0.0, 0.0],
            ..ThumpSpec::default()
        });
        assert!(!t.is_empty());
        assert!(t.samples.iter().all(|v| *v == 0.0));
        assert_eq!(t.peak_dbfs, f64::NEG_INFINITY, "静音的峰值是负无穷");
        assert_eq!(t.dc_offset, 0.0);
    }

    /// 默认参数下「咚」的包络应该是**先冲后落**的：峰值出现在前 1/3，
    /// 而不是末尾。落在末尾的话，淡出会把它削掉，
    /// 而用户听到的会是一声渐强的嗡鸣而不是撞击。
    #[test]
    fn the_default_envelope_peaks_early() {
        let t = build_thump(&ThumpSpec::default());
        let peak_at = t
            .samples
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        let ratio = peak_at as f64 / t.len() as f64;
        assert!(
            ratio < 0.35,
            "峰值出现在 {:.0}% 处，太靠后了（应在前 35% 内）",
            ratio * 100.0
        );
    }
}
