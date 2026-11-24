//! 设备样本格式 → 单声道 f32。
//!
//! # 为什么这件事值得单独一个模块
//!
//! 它跑在两个实时回调**里面**，是整个程序里唯一对每一个样本都要执行的
//! 代码。同时它有 12 种输入格式 × 任意声道数的组合，而其中只有你手上
//! 那台设备的组合会被日常用到——**其余的错了也不会有人发现**，直到有
//! 人换了支 USB 麦克风，电平莫名其妙差了 6 dB 或者整条链路全是噪声。
//!
//! 所以这里的所有格式都有测试，包括本机永远不会走的那几种。
//!
//! # 归一化约定
//!
//! 定点和浮点统一到 **±1.0**，与 `dBFS` 的零点一致：
//!
//! | 格式 | 满刻度 | 归一化 |
//! |---|---|---|
//! | `i8` | ±128 | ÷ 2⁷ |
//! | `i16` | ±32768 | ÷ 2¹⁵ |
//! | `i24` | ±8388608 | ÷ 2²³ |
//! | `i32` | ±2147483648 | ÷ 2³¹ |
//! | `i64` | ±2⁶³ | ÷ 2⁶³ |
//! | `u8`…`u64` | 同上，先减去中点 | 同上 |
//! | `f32` / `f64` | ±1.0 | 原样 |
//!
//! 用 2^(n−1) 而不是 `T::MAX` 作除数：`i16::MIN` 的绝对值比 `i16::MAX`
//! 大 1，用 `MAX` 会让负半周被截到 −1.0 上，产生一个只在负峰出现的削波。
//! 这个差异在满刻度的 −0.0003 dB，但它是**系统性**的。
//!
//! 代价是定点格式的正满刻度到不了 +1.0——它是 `1 − 2^−(n−1)`，
//! `i8` 是 0.9921875、`i16` 是 0.9999695。这是除法约定的必然结果，
//! 不是缺陷；**看到它别去「修」**，修了就会引入上面那个削波。
//!
//! # 降混用算术平均
//!
//! 除以声道数，不是乘以 `1/√N`。结构声在相邻声道里是**高度相关**的，
//! 相关信号相加再除以 N 才保持幅度；用 `1/√N` 会让同一个声音在立体声
//! 设备上比单声道设备高 3 dB，于是「换个麦克风，所有历史读数都不可比」。
//!
//! # 最后一道防线
//!
//! 回调里收到的浮点样本可能是 NaN 或超出 ±1.0——驱动有 bug、设备在
//! 拔出的瞬间吐垃圾、共享模式下别的进程在重采样。**NaN 一旦进入 IIR
//! 滤波器就永远不会自己消失**（`NaN` 经任何乘加还是 `NaN`），几秒钟后
//! 整条链路全是 `NaN`，而 UI 上只表现为「仪表不动了」。所以这里就地
//! 清洗，代价是一次比较。

use cpal::{FromSample, Sample};

/// 单声道缓冲的默认容量，帧。
///
/// 16384 帧 = 341 ms @ 48 kHz，远超任何真实设备给出的块大小（WASAPI
/// 共享模式典型 480–1920 帧，CoreAudio 更小，ASIO 可能到 4096）。
/// 超出时调用方按这个容量分块处理，而不是扩容——**扩容就是分配**，
/// 而分配在实时回调里是禁止的。
pub const DEFAULT_CAPACITY_FRAMES: usize = 16_384;

/// 按声道降混到单声道 f32。
pub struct Downmixer {
    channels: usize,
    scratch: Vec<f32>,
}

impl Downmixer {
    /// `channels` 必须 ≥ 1。
    ///
    /// 声道数在流建立时固定，中途不会变——所以这里可以放心地把它当作
    /// 常量，不需要每次调用都检查。
    pub fn new(channels: usize, capacity_frames: usize) -> Self {
        Self {
            channels: channels.max(1),
            scratch: vec![0.0; capacity_frames.max(1)],
        }
    }

    /// 一次能降混多少帧。
    pub fn capacity_frames(&self) -> usize {
        self.scratch.len()
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// 一次能吃掉多少**样本**（含所有声道）。
    pub fn capacity_samples(&self) -> usize {
        self.scratch.len() * self.channels
    }

    /// 降混 `input` 并返回单声道切片。
    ///
    /// `input.len()` 必须是 `channels` 的整数倍；不是整数倍时末尾那个
    /// 残缺帧被丢弃（半帧凑不出一个样本）。返回长度即 `input.len() / channels`。
    ///
    /// 调用方负责保证帧数不超过 [`Self::capacity_frames`]。
    pub fn downmix_into<T>(&mut self, input: &[T]) -> &[f32]
    where
        T: Sample,
        // dasp 的转换是 `FromSample` 而不是 `Into`——方向搞反的话
        // 这里会编译失败，而不是在运行时悄悄换个约定。
        f32: FromSample<T>,
    {
        let ch = self.channels;
        let frames = (input.len() / ch).min(self.scratch.len());
        let inv = 1.0 / ch as f32;

        if ch == 1 {
            for (dst, src) in self.scratch[..frames].iter_mut().zip(input.iter()) {
                *dst = sanitize(src.to_sample::<f32>());
            }
        } else {
            for (f, dst) in self.scratch[..frames].iter_mut().enumerate() {
                let base = f * ch;
                let mut acc = 0.0f32;
                for c in 0..ch {
                    acc += sanitize(input[base + c].to_sample::<f32>());
                }
                // 求和后再乘倒数：逐项相乘会累积舍入，而声道数多的时候
                // （8 声道接口）这个偏差是可以测出来的。
                *dst = acc * inv;
            }
        }
        &self.scratch[..frames]
    }
}

/// 把任意浮点样本清成 ±1.0 内的有限值。
///
/// `!(x > -1.0)` 而不是 `x <= -1.0`：前者同时挡下 `NaN`。
#[inline]
fn sanitize(x: f32) -> f32 {
    if !(x > -1.0) {
        // 含 NaN 与 -inf
        if x.is_nan() {
            0.0
        } else {
            -1.0
        }
    } else if !(x < 1.0) {
        // 含 +inf
        if x.is_nan() {
            0.0
        } else {
            1.0
        }
    } else {
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 满刻度必须映射到 ±1.0，静音映射到 0.0。12 种格式逐个过一遍。
    ///
    /// 这几条覆盖的组合本机永远不会全部走到——正是因此才需要测试。
    ///
    /// # 定点格式的满刻度是不对称的
    ///
    /// 补码的最小值比最大值多一格：`i8` 是 −128..127。所以
    ///
    /// * 最小值 → 恰好 **−1.0**
    /// * 最大值 → **1 − 2^−(n−1)**，`i8` 是 0.9921875，`i16` 是 0.9999695
    ///
    /// 这不是缺陷，是除法约定为 2^(n−1) 的必然结果。断言必须按这个来，
    /// 否则会误以为「差了一点点」是 bug，然后「修」成除以 `T::MAX`——
    /// 那才会引入真正的负半周削波。
    #[test]
    fn every_format_maps_full_scale_to_one() {
        /// `$step` = 2^-(有效位数-1)，即最大值的理论值 `1 - step`。
        macro_rules! check {
            ($t:ty, $step:expr, $max:expr, $min:expr, $mid:expr) => {{
                let mut d = Downmixer::new(1, 8);
                let got = d.downmix_into(&[$max as $t, $min as $t, $mid as $t]);
                let want_max = 1.0 - $step;
                assert!(
                    (got[0] - want_max).abs() < 1e-9,
                    "{} 正满刻度 → {}（应为 1 − 2^-n = {}）",
                    stringify!($t),
                    got[0],
                    want_max
                );
                assert_eq!(
                    got[1], -1.0,
                    "{} 负满刻度 → {}（补码最负值应恰好是 -1.0）",
                    stringify!($t),
                    got[1]
                );
                assert_eq!(got[2], 0.0, "{} 中点 → {}（应为 0.0）", stringify!($t), got[2]);
            }};
        }

        check!(i8, 1.0 / 128.0, i8::MAX, i8::MIN, 0i8);
        check!(i16, 1.0 / 32768.0, i16::MAX, i16::MIN, 0i16);
        check!(i32, 1.0 / 2147483648.0, i32::MAX, i32::MIN, 0i32);
        check!(i64, 1.0 / 9223372036854775808.0, i64::MAX, i64::MIN, 0i64);
        check!(u8, 1.0 / 128.0, u8::MAX, u8::MIN, 128u8);
        check!(u16, 1.0 / 32768.0, u16::MAX, u16::MIN, 32768u16);
        check!(u32, 1.0 / 2147483648.0, u32::MAX, u32::MIN, 1u32 << 31);
        check!(u64, 1.0 / 9223372036854775808.0, u64::MAX, u64::MIN, 1u64 << 63);
        check!(f32, 0.0, 1.0f32, -1.0f32, 0.0f32);
        check!(f64, 0.0, 1.0f64, -1.0f64, 0.0f64);

        // 24 位格式：4 字节存储、24 位有效。数值按定义写出来而不是用
        // dasp 的常量——它的 `MIN`/`MAX` 是模块级 const 而不是关联常量，
        // 而这里的重点恰恰是**这两个数**是什么。
        const I24_MAX: i32 = 8_388_607; // 2^23 - 1
        const I24_MIN: i32 = -8_388_608; // -2^23
        const U24_MAX: i32 = 16_777_215; // 2^24 - 1
        const U24_MIN: i32 = 0;
        const U24_EQ: i32 = 8_388_608; // 2^23

        let mut d = Downmixer::new(1, 8);
        let got = d.downmix_into(&[
            cpal::I24::from(I24_MAX),
            cpal::I24::from(I24_MIN),
            cpal::I24::from(0),
        ]);
        assert!(
            (got[0] - (1.0 - 1.0 / 8_388_608.0)).abs() < 1e-9,
            "i24 正满刻度 → {}",
            got[0]
        );
        assert_eq!(got[1], -1.0, "i24 负满刻度 → {}", got[1]);
        assert_eq!(got[2], 0.0, "i24 中点 → {}", got[2]);

        let mut d = Downmixer::new(1, 8);
        let got = d.downmix_into(&[
            cpal::U24::from(U24_MAX),
            cpal::U24::from(U24_MIN),
            cpal::U24::from(U24_EQ),
        ]);
        assert!(
            (got[0] - (1.0 - 1.0 / 8_388_608.0)).abs() < 1e-9,
            "u24 正满刻度 → {}",
            got[0]
        );
        assert_eq!(got[1], -1.0, "u24 负满刻度 → {}", got[1]);
        assert_eq!(got[2], 0.0, "u24 中点 → {}", got[2]);
    }

    /// 正负半周必须对称。用 `T::MAX` 当除数的话，负峰会先于正峰削波，
    /// 而那只表现为一点点额外的失真，几乎不可能被听出来或看出来。
    #[test]
    fn positive_and_negative_full_scale_are_symmetric() {
        let mut d = Downmixer::new(1, 4);
        let got = d.downmix_into(&[i16::MAX, i16::MIN]);
        let pos = got[0];
        let neg = -got[1];
        assert!(
            (pos - neg).abs() < 0.001,
            "正峰 {pos} 与负峰 {neg} 不对称，差了 {:.4} dB",
            (pos / neg).log10() * 20.0
        );
    }

    /// 立体声里**相同**的信号降混后幅度不变。
    ///
    /// 这是「除以 N」而不是「除以 √N」的判据：结构声在相邻声道里高度
    /// 相关，用 `1/√N` 会让立体声设备比单声道设备高 3 dB。
    #[test]
    fn correlated_stereo_keeps_its_amplitude() {
        let mut d = Downmixer::new(2, 8);
        let got = d.downmix_into(&[0.5f32, 0.5, -0.25, -0.25, 0.0, 0.0]);
        assert_eq!(got.len(), 3);
        assert!((got[0] - 0.5).abs() < 1e-6, "实得 {}", got[0]);
        assert!((got[1] + 0.25).abs() < 1e-6, "实得 {}", got[1]);
    }

    /// 完全反相的立体声（一正一负）平均后是零。不是 bug——那种信号在
    /// 单声道里本来就不存在，而假装它存在才是 bug。
    #[test]
    fn anti_phase_stereo_cancels() {
        let mut d = Downmixer::new(2, 4);
        let got = d.downmix_into(&[0.5f32, -0.5]);
        assert_eq!(got[0], 0.0);
    }

    /// 声道数多了也不能溢出：8 声道满刻度求和是 8.0，除以 8 之后是 1.0。
    #[test]
    fn eight_channel_full_scale_does_not_clip() {
        let mut d = Downmixer::new(8, 4);
        let input = [1.0f32; 8];
        let got = d.downmix_into(&input);
        assert!((got[0] - 1.0).abs() < 1e-6, "实得 {}", got[0]);
    }

    /// 残缺帧要丢掉，不能把上一个声道的数据当成新帧的开头。
    #[test]
    fn partial_frames_are_dropped() {
        let mut d = Downmixer::new(2, 8);
        // 5 个样本 = 2 整帧 + 1 个残片
        let got = d.downmix_into(&[0.1f32, 0.1, 0.2, 0.2, 0.9]);
        assert_eq!(got.len(), 2, "5 个样本在 2 声道下应得到 2 帧");
        assert!((got[1] - 0.2).abs() < 1e-6);
    }

    /// **NaN 必须在进滤波器之前被清掉。**
    ///
    /// 一个 NaN 样本会让整个 IIR 链的输出永久变成 NaN——它不会自己
    /// 衰减掉，因为在数学上 `NaN` 参与的运算是 `NaN`。UI 上只表现为
    /// 「仪表不动了」，而那时录音和检测已经全是废的。
    #[test]
    fn nan_and_infinity_are_scrubbed() {
        let mut d = Downmixer::new(1, 8);
        let got = d.downmix_into(&[
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
        ]);
        assert_eq!(got[0], 0.0, "NaN 应为 0，实得 {}", got[0]);
        assert_eq!(got[1], 1.0, "+inf 应钳到 +1，实得 {}", got[1]);
        assert_eq!(got[2], -1.0, "-inf 应钳到 -1，实得 {}", got[2]);
        assert_eq!(got[3], 0.0, "NaN 应为 0，实得 {}", got[3]);
        assert!(got.iter().all(|v| v.is_finite()));
    }

    /// 超出 ±1.0 的浮点样本（共享模式下别的进程在重采样时会出现）钳住
    /// 而不是放行。
    #[test]
    fn out_of_range_floats_are_clamped() {
        let mut d = Downmixer::new(1, 4);
        let got = d.downmix_into(&[1.5f32, -3.0, 0.5]);
        assert_eq!(got[0], 1.0);
        assert_eq!(got[1], -1.0);
        assert_eq!(got[2], 0.5);
    }

    /// 声道数为 0 是设备汇报的脏数据，不能让它变成除零。
    #[test]
    fn zero_channels_is_coerced_to_mono() {
        let mut d = Downmixer::new(0, 4);
        assert_eq!(d.channels(), 1);
        let got = d.downmix_into(&[0.25f32]);
        assert_eq!(got[0], 0.25);
    }

    /// 超过容量的输入不能越界，也不能扩容——只能截断。
    #[test]
    fn input_beyond_capacity_is_truncated_not_reallocated() {
        let mut d = Downmixer::new(1, 4);
        let cap_before = d.capacity_frames();
        let big = vec![0.5f32; 1000];
        let got = d.downmix_into(&big);
        assert_eq!(got.len(), 4);
        assert_eq!(d.capacity_frames(), cap_before, "容量不该变化（那就是分配）");
    }

    /// 定点正弦的 dBFS 必须与解析值对上。这是「归一化约定没搞错」的
    /// 端到端判据——上面那些逐点检查看不出 2^n 和 2^n−1 的区别。
    #[test]
    fn quantized_sine_matches_its_analytic_dbfs() {
        let amp = 0.5f32;
        let n = 48_000;
        let full = (i32::MAX as f64) + 1.0; // 2^31
        let sine: Vec<i32> = (0..n)
            .map(|i| {
                let v = amp as f64 * (std::f64::consts::TAU * 1000.0 * i as f64 / 48_000.0).sin();
                (v * full).round() as i32
            })
            .collect();

        let mut d = Downmixer::new(1, n);
        let mono = d.downmix_into(&sine).to_vec();

        let ms: f64 = mono.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / n as f64;
        let db = 10.0 * ms.log10();
        let expected = 10.0 * ((amp as f64).powi(2) / 2.0).log10();
        assert!(
            (db - expected).abs() < 0.01,
            "量化正弦的 dBFS 应为 {expected:.3}，实测 {db:.3}"
        );
    }
}
