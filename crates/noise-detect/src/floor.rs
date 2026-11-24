//! 自适应底噪。
//!
//! # 为什么必须自适应
//!
//! 固定阈值在任何真实住宅里都会很快失效：夜里 25 dB 的房间里合格的阈值，
//! 白天开着空调时就完全不触发。底噪必须跟着环境走。
//!
//! # 非对称时间常数
//!
//! **升快降慢。** 两个方向为什么不对称：
//!
//! - **升得相对快**（30 s）：空调/车流这类持续噪声起来之后，底噪要尽快
//!   把它吸收掉，否则它会一直悬在阈值之上。
//! - **降得慢**（300 s）：房间安静下来时**不能**立刻恢复高灵敏度。
//!   邻居停下脚步的那一瞬间，底噪若还在高位，紧接着的第一步就会
//!   被当成巨大冲击。慢降给了系统一个缓冲期。
//!
//! # 只在 Idle 态自适应
//!
//! 调用方（`sm::Detector`）只在 Idle 态调用 [`AdaptiveNoiseFloor::update`]，
//! 事件进行中和门控挂起时调用 [`AdaptiveNoiseFloor::freeze`]。这条纪律是
//! 半双工不变量的一部分：**自己的回放尾音永远不会被学进底噪**。

// 均方值 ↔ dB 的转换只有**一处**实现，在 `noise-dsp::features` 里。
// 这里重新导出而不是各写一份：两份实现早晚会在下限、参考定义或
// NaN 行为上分叉，而分叉的表现是「同一帧在检测器和特征里显示不同的
// dB」——一个查起来极其痛苦的 bug。
pub use noise_dsp::features::{db_to_mean_square, mean_square_to_db_floored, DB_FLOOR};

/// 自适应底噪，以**功率**（均方值）为状态量。
///
/// 在功率域做指数平均而不是在 dB 域：连续噪声的功率是可叠加的物理量，
/// 而 dB 的平均在数学上没有对应的物理意义。非对称性通过**选择哪个
/// 时间常数**来实现，而不是在 dB 域上做手脚。
#[derive(Clone, Debug)]
pub struct AdaptiveNoiseFloor {
    /// 当前底噪功率（均方值，与特征同参考系）。
    power: f64,
    tau_up_s: f64,
    tau_down_s: f64,
    /// 预热剩余帧数。
    warmup_frames_left: u64,
    warmup_tau_s: f64,
    frames_per_second: f64,
    frozen: bool,
    /// 是否已收到过第一帧。
    initialized: bool,
}

impl AdaptiveNoiseFloor {
    /// * `frames_per_second` —— 每秒的分析帧数（帧长 10 ms 时为 100）
    pub fn new(frames_per_second: f64, tau_up_s: f64, tau_down_s: f64) -> Self {
        Self {
            power: 0.0,
            tau_up_s,
            tau_down_s,
            warmup_frames_left: 0,
            warmup_tau_s: tau_up_s,
            frames_per_second,
            frozen: false,
            initialized: false,
        }
    }

    /// 设置预热时长与预热期间的时间常数。
    ///
    /// 没有预热的话，应用在空调已经开着的房间里启动，底噪会锁在启动瞬间
    /// 的高位，然后用 300 s 慢慢降下来——用户前五分钟什么都检测不到。
    pub fn with_warmup(mut self, warmup_s: f64, warmup_tau_s: f64) -> Self {
        self.warmup_frames_left = (warmup_s * self.frames_per_second).round() as u64;
        self.warmup_tau_s = warmup_tau_s;
        self
    }

    /// 用一帧的均方值更新底噪，返回更新后的底噪 dB。
    ///
    /// 冻结期间返回当前值，不做任何更新。
    pub fn update(&mut self, frame_mean_square: f64, reference: f64) -> f64 {
        if self.frozen {
            return self.value_db(reference);
        }

        // 非有限输入直接忽略，但**不**改变状态——一个 NaN 帧不应该
        // 永久污染底噪估计。
        if !frame_mean_square.is_finite() || frame_mean_square < 0.0 {
            return self.value_db(reference);
        }

        if !self.initialized {
            // 第一帧直接作为初值。若首帧恰好是静音，用 0 会让底噪锁死在
            // 下限，所以给它一个极小的非零值。
            self.power = frame_mean_square.max(f64::MIN_POSITIVE);
            self.initialized = true;
            return self.value_db(reference);
        }

        // 预热期间双向都用快时间常数：快升快降才能让底噪在两三秒内
        // 贴住环境，而不是用 300 s 从启动瞬间的读数慢慢爬下来。
        let tau = if self.warmup_frames_left > 0 {
            self.warmup_frames_left -= 1;
            self.warmup_tau_s
        } else if frame_mean_square > self.power {
            self.tau_up_s
        } else {
            self.tau_down_s
        };

        let alpha = 1.0 - (-1.0 / (self.frames_per_second * tau)).exp();
        self.power += alpha * (frame_mean_square - self.power);
        // 底噪功率不允许真的变成 0，否则 dB 会落到下限并失去跟踪能力。
        self.power = self.power.max(f64::MIN_POSITIVE);

        self.value_db(reference)
    }

    /// 当前底噪，dB。
    pub fn value_db(&self, reference: f64) -> f64 {
        mean_square_to_db_floored(self.power, reference)
    }

    /// 当前底噪的均方值。
    pub fn value_power(&self) -> f64 {
        self.power
    }

    /// 是否已经收到过至少一帧。
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// 冻结：停止自适应，保持当前值。
    pub fn freeze(&mut self) {
        self.frozen = true;
    }

    /// 解冻。
    pub fn unfreeze(&mut self) {
        self.frozen = false;
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    pub fn reset(&mut self) {
        self.power = 0.0;
        self.frozen = false;
        self.initialized = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REF: f64 = 1.0;

    #[test]
    fn first_frame_seeds_the_floor() {
        let mut f = AdaptiveNoiseFloor::new(100.0, 30.0, 300.0);
        assert!(!f.is_initialized());
        let db = f.update(0.01, REF);
        assert!(f.is_initialized());
        assert!((db - (-20.0)).abs() < 1e-9, "实得 {db}");
    }

    /// 底噪在**功率域**做指数平均，所以「一个时间常数后走完 63% 的路」
    /// 是说**功率**走完 63%，不是 dB 走完 63%。
    ///
    /// 两者差得很远，而直觉会强烈地倒向错误的那一个：从 -40 dB 阶跃到
    /// -20 dB，dB 域的答案是 -40 + 0.632×20 = **-27.4 dB**，功率域的正确
    /// 答案是 **-22.0 dB**——相差 5.4 dB，比这道题的容差还大。所以这里
    /// 从闭式解重算期望值，并显式断言它**不等于** dB 域的直觉答案。
    #[test]
    fn floor_rises_toward_a_step_up_in_the_power_domain() {
        let fps = 100.0;
        let tau = 30.0;
        let n = (fps * tau) as usize; // 正好一个时间常数
        let (p_from, p_to) = (1e-4, 1e-2); // -40 dB → -20 dB

        let mut f = AdaptiveNoiseFloor::new(fps, tau, 300.0);
        f.update(p_from, REF);
        for _ in 0..n {
            f.update(p_to, REF);
        }

        // 闭式解：p = p_to + (p_from − p_to)·e^(−1)
        let expected_power = p_to + (p_from - p_to) * (-1.0f64).exp();
        let expected_db = 10.0 * expected_power.log10();

        let db = f.value_db(REF);
        assert!(
            (db - expected_db).abs() < 0.05,
            "一个时间常数后应为 {expected_db:.2} dB（功率域），实得 {db:.2} dB"
        );

        let naive_db_domain = -40.0 + (1.0 - (-1.0f64).exp()) * 20.0; // -27.4
        assert!(
            (db - naive_db_domain).abs() > 4.0,
            "底噪**不该**按 dB 域插值（那会给出 {naive_db_domain:.1} dB）；\
             实得 {db:.2} dB，两者的差距正是这个测试要钉住的东西"
        );
    }

    #[test]
    fn floor_falls_slower_than_it_rises() {
        // 上升与下降用同样的阶跃幅度，比较同等时间后的移动量
        let mut up = AdaptiveNoiseFloor::new(100.0, 30.0, 300.0);
        let mut down = AdaptiveNoiseFloor::new(100.0, 30.0, 300.0);
        up.update(1e-4, REF);
        down.update(1e-2, REF);
        for _ in 0..(100 * 30) {
            up.update(1e-2, REF);
            down.update(1e-4, REF);
        }
        let rise = up.value_db(REF) - (-40.0);
        let fall = (-20.0) - down.value_db(REF);
        assert!(
            rise > fall * 5.0,
            "同等时间内上升量({rise:.2} dB)应远大于下降量({fall:.2} dB)"
        );
    }

    #[test]
    fn frozen_floor_does_not_move() {
        let mut f = AdaptiveNoiseFloor::new(100.0, 1.0, 300.0);
        f.update(1e-4, REF);
        f.freeze();
        let before = f.value_db(REF);
        for _ in 0..10_000 {
            f.update(1.0, REF); // 疯狂喂大信号
        }
        assert_eq!(f.value_db(REF), before, "冻结期间底噪必须纹丝不动");
        assert!(f.is_frozen());

        f.unfreeze();
        f.update(1.0, REF);
        assert!(f.value_db(REF) > before, "解冻后应恢复跟踪");
    }

    /// 自己的回放尾音绝不会被学进底噪——这条由「冻结」保证，
    /// 这里显式验证冻结确实拦住了它。
    #[test]
    fn playback_tail_cannot_be_learned_while_frozen() {
        let mut f = AdaptiveNoiseFloor::new(100.0, 30.0, 300.0);
        f.update(1e-6, REF); // 安静房间
        let quiet_db = f.value_db(REF);

        f.freeze();
        // 模拟 800 ms 的回放尾音
        for _ in 0..80 {
            f.update(0.5, REF);
        }
        f.unfreeze();
        assert!(
            (f.value_db(REF) - quiet_db).abs() < 1e-9,
            "回放不应改变底噪，{quiet_db} → {}",
            f.value_db(REF)
        );
    }

    #[test]
    fn warmup_settles_faster_than_normal_decay() {
        let mut warm = AdaptiveNoiseFloor::new(100.0, 30.0, 300.0).with_warmup(10.0, 3.0);
        let mut plain = AdaptiveNoiseFloor::new(100.0, 30.0, 300.0);
        // 两者都从高位起（模拟「空调已经开着时启动应用」）
        warm.update(1e-2, REF);
        plain.update(1e-2, REF);
        // 10 秒后环境其实是安静的
        for _ in 0..1000 {
            warm.update(1e-6, REF);
            plain.update(1e-6, REF);
        }
        assert!(
            warm.value_db(REF) < plain.value_db(REF) - 3.0,
            "预热应让底噪降得更快：预热 {:.1} dB vs 常规 {:.1} dB",
            warm.value_db(REF),
            plain.value_db(REF)
        );
    }

    #[test]
    fn nan_input_does_not_poison_the_floor() {
        let mut f = AdaptiveNoiseFloor::new(100.0, 30.0, 300.0);
        f.update(1e-4, REF);
        let before = f.value_db(REF);
        f.update(f64::NAN, REF);
        f.update(f64::INFINITY, REF);
        f.update(-1.0, REF);
        assert!(f.value_db(REF).is_finite());
        assert_eq!(f.value_db(REF), before, "非法输入不应改变底噪");
    }

    #[test]
    fn silence_does_not_collapse_the_floor_to_negative_infinity() {
        let mut f = AdaptiveNoiseFloor::new(100.0, 30.0, 300.0);
        f.update(0.0, REF);
        for _ in 0..10_000 {
            f.update(0.0, REF);
        }
        assert!(f.value_db(REF).is_finite());
        assert!(f.value_db(REF) >= DB_FLOOR);
    }

    /// 底噪的 dB 报告必须与特征侧的转换一致——两者分叉是本文件
    /// 特意把实现挪走的原因，这里钉住它。
    #[test]
    fn floor_reports_db_through_the_shared_conversion() {
        let mut f = AdaptiveNoiseFloor::new(100.0, 30.0, 300.0);
        f.update(0.5, REF);
        assert_eq!(
            f.value_db(REF),
            noise_dsp::features::mean_square_to_db_floored(0.5, REF)
        );
    }

    #[test]
    fn reset_clears_everything() {
        let mut f = AdaptiveNoiseFloor::new(100.0, 30.0, 300.0);
        f.update(1e-2, REF);
        f.freeze();
        f.reset();
        assert!(!f.is_initialized());
        assert!(!f.is_frozen());
        // 重新初始化后应接受新环境的初值
        f.update(1e-6, REF);
        assert!((f.value_db(REF) - (-60.0)).abs() < 1e-9);
    }
}
