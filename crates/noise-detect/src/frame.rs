//! 分析帧装配。
//!
//! 把任意大小的设备缓冲块累积成**固定长度**的分析帧，使检测器的时基与
//! 音频回调的缓冲大小解耦。
//!
//! # 为什么必须解耦
//!
//! cpal 给的块长由驱动决定：512、480、1024、甚至每次都不一样。检测器里
//! 的每一个时间量（上升沿窗口 5 帧 = 50 ms、hangover 800 ms、底噪时间
//! 常数）都以「帧」为单位。如果帧长跟着驱动走，**同一段录音在不同机器上
//! 会得到不同的检测结果**，而且换一个缓冲大小就要重新调参。
//!
//! 所以帧长在这里被钉死为 `cfg.frame_ms`，块边界只是被无声地跨过去。

/// 一帧的统计量。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameStats {
    /// 自会话起点的帧序号。
    pub frame_index: u64,
    /// 自会话起点的毫秒数（帧**起点**的时刻）。
    pub t_ms: f64,
    /// 帧内均方值。
    pub mean_square: f64,
    /// 帧内绝对值峰值。
    pub peak: f64,
}

impl FrameStats {
    /// 帧内均方值 → dB（相对给定参考）。
    ///
    /// 静音返回一个有限的下限值而不是 `-inf`：下游的状态机、滑动窗口
    /// 最大值、以及各种差值运算都不该被迫处理无穷大。
    pub fn level_db(&self, reference: f64) -> f64 {
        crate::floor::mean_square_to_db_floored(self.mean_square, reference)
    }
}

/// 把任意块长累积成固定长度帧。
///
/// 每凑满一帧就调用一次 `emit`。跨越块边界的帧会正确地拼接起来，
/// 残尾留到下一次 `push` 继续累积。
pub struct FrameAssembler {
    frame_len: usize,
    frame_ms: f64,
    /// 当前帧内已累计的平方和与峰值。
    acc_sq: f64,
    acc_peak: f64,
    /// 当前帧内已填入的样本数。
    filled: usize,
    /// 下一帧的序号。
    next_index: u64,
}

impl FrameAssembler {
    /// * `fs` —— 实际达成的采样率（**不是**请求的采样率）
    /// * `frame_ms` —— 帧长，毫秒
    pub fn new(fs: f64, frame_ms: f64) -> Self {
        let frame_len = ((fs * frame_ms / 1000.0).round() as usize).max(1);
        Self {
            frame_len,
            frame_ms: frame_len as f64 * 1000.0 / fs,
            acc_sq: 0.0,
            acc_peak: 0.0,
            filled: 0,
            next_index: 0,
        }
    }

    /// 每帧的样本数。
    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    /// 实际的帧长（毫秒）。与请求值可能有微小出入——`frame_len` 是
    /// 整数个样本，除不尽时会被圆整。这个值是**真实时基**，事件时间戳
    /// 要按它算。
    pub fn frame_ms(&self) -> f64 {
        self.frame_ms
    }

    /// 已经产出的帧数。
    pub fn frames_emitted(&self) -> u64 {
        self.next_index
    }

    /// 喂入一个块。对每个凑满的帧调用 `emit`。
    pub fn push(&mut self, x: &[f64], mut emit: impl FnMut(FrameStats)) {
        let mut i = 0;
        while i < x.len() {
            let take = (self.frame_len - self.filled).min(x.len() - i);
            for &s in &x[i..i + take] {
                self.acc_sq += s * s;
                let a = s.abs();
                if a > self.acc_peak {
                    self.acc_peak = a;
                }
            }
            self.filled += take;
            i += take;

            if self.filled == self.frame_len {
                emit(self.take_frame());
            }
        }
    }

    /// 把当前累积的内容作为一帧取走并复位。
    fn take_frame(&mut self) -> FrameStats {
        let stats = FrameStats {
            frame_index: self.next_index,
            t_ms: self.next_index as f64 * self.frame_ms,
            mean_square: self.acc_sq / self.frame_len as f64,
            peak: self.acc_peak,
        };
        self.next_index += 1;
        self.acc_sq = 0.0;
        self.acc_peak = 0.0;
        self.filled = 0;
        stats
    }

    /// 丢弃未凑满的残帧。
    ///
    /// 回放门控的硬静音期要用它：那段时间麦克风收到的是自己放的声音，
    /// 既不该计入任何一帧，也不该让残尾带着自己的回放能量混进下一帧。
    pub fn discard_partial(&mut self) {
        self.acc_sq = 0.0;
        self.acc_peak = 0.0;
        self.filled = 0;
    }

    pub fn reset(&mut self) {
        self.discard_partial();
        self.next_index = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(asm: &mut FrameAssembler, x: &[f64]) -> Vec<FrameStats> {
        let mut out = Vec::new();
        asm.push(x, |f| out.push(f));
        out
    }

    #[test]
    fn frame_len_rounds_to_whole_samples() {
        // 48 kHz / 10 ms = 480 样本，整除
        assert_eq!(FrameAssembler::new(48_000.0, 10.0).frame_len(), 480);
        // 44.1 kHz / 10 ms = 441，也整除
        assert_eq!(FrameAssembler::new(44_100.0, 10.0).frame_len(), 441);
        // 8 kHz / 10 ms = 80
        assert_eq!(FrameAssembler::new(8_000.0, 10.0).frame_len(), 80);
    }

    #[test]
    fn non_integer_frame_len_is_rounded_and_reported() {
        // 22.05 kHz / 7 ms = 154.35 → 154 样本
        let asm = FrameAssembler::new(22_050.0, 7.0);
        assert_eq!(asm.frame_len(), 154);
        // 实际帧长必须回报为 154 样本对应的真实时长，而不是请求的 7.0
        let expected = 154.0 * 1000.0 / 22_050.0;
        assert!((asm.frame_ms() - expected).abs() < 1e-12);
        assert!((asm.frame_ms() - 7.0).abs() > 1e-9, "必须暴露圆整后的真实时基");
    }

    #[test]
    fn exact_multiple_produces_exact_frame_count() {
        let mut asm = FrameAssembler::new(48_000.0, 10.0);
        let frames = collect(&mut asm, &vec![0.5; 4800]);
        assert_eq!(frames.len(), 10);
    }

    /// **核心性质**：切块方式不影响帧的切分。
    #[test]
    fn chunk_boundaries_do_not_affect_framing() {
        let x: Vec<f64> = (0..48_000).map(|i| (i as f64 * 0.001).sin()).collect();

        let mut whole = FrameAssembler::new(48_000.0, 10.0);
        let a = collect(&mut whole, &x);

        for chunk in [1usize, 7, 100, 480, 512, 1000, 4096] {
            let mut asm = FrameAssembler::new(48_000.0, 10.0);
            let mut b = Vec::new();
            for c in x.chunks(chunk) {
                asm.push(c, |f| b.push(f));
            }
            assert_eq!(a.len(), b.len(), "块长 {chunk} 改变了帧数");
            for (fa, fb) in a.iter().zip(b.iter()) {
                assert_eq!(fa.frame_index, fb.frame_index, "块长 {chunk} 帧序号不符");
                assert!(
                    (fa.mean_square - fb.mean_square).abs() < 1e-12,
                    "块长 {chunk} 帧 {fa:?} 的均方值不符：{} vs {}",
                    fa.mean_square,
                    fb.mean_square
                );
                assert!((fa.t_ms - fb.t_ms).abs() < 1e-12);
            }
        }
    }

    #[test]
    fn partial_frame_is_carried_over_not_dropped() {
        // 480 样本一帧，喂 700 → 只出 1 帧，剩 220 留在装配器里
        let mut asm = FrameAssembler::new(48_000.0, 10.0);
        let frames = collect(&mut asm, &vec![1.0; 700]);
        assert_eq!(frames.len(), 1);
        // 再喂 260 → 220 + 260 = 480，出第 2 帧
        let frames = collect(&mut asm, &vec![1.0; 260]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].frame_index, 1);
        assert!((frames[0].mean_square - 1.0).abs() < 1e-12);
    }

    #[test]
    fn mean_square_and_peak_are_correct() {
        let mut asm = FrameAssembler::new(48_000.0, 10.0);
        let mut x = vec![0.0; 480];
        x[100] = -0.8;
        x[200] = 0.3;
        let frames = collect(&mut asm, &x);
        assert_eq!(frames.len(), 1);
        assert!((frames[0].peak - 0.8).abs() < 1e-12);
        let expected_ms = (0.64 + 0.09) / 480.0;
        assert!((frames[0].mean_square - expected_ms).abs() < 1e-15);
    }

    #[test]
    fn timestamps_advance_with_frame_index() {
        let mut asm = FrameAssembler::new(48_000.0, 10.0);
        let frames = collect(&mut asm, &vec![0.0; 1440]);
        assert_eq!(frames.len(), 3);
        assert!((frames[0].t_ms - 0.0).abs() < 1e-12);
        assert!((frames[1].t_ms - 10.0).abs() < 1e-12);
        assert!((frames[2].t_ms - 20.0).abs() < 1e-12);
    }

    #[test]
    fn discard_partial_drops_the_residue() {
        let mut asm = FrameAssembler::new(48_000.0, 10.0);
        let _ = collect(&mut asm, &vec![1.0; 700]);
        asm.discard_partial();
        // 残尾被丢掉，补 480 个静音应该出 1 帧，且均方值为 0
        let frames = collect(&mut asm, &vec![0.0; 480]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].mean_square, 0.0);
    }

    #[test]
    fn reset_restarts_frame_numbering() {
        let mut asm = FrameAssembler::new(48_000.0, 10.0);
        let _ = collect(&mut asm, &vec![0.0; 960]);
        assert_eq!(asm.frames_emitted(), 2);
        asm.reset();
        assert_eq!(asm.frames_emitted(), 0);
        let frames = collect(&mut asm, &vec![0.0; 480]);
        assert_eq!(frames[0].frame_index, 0);
    }

    #[test]
    fn empty_input_produces_nothing() {
        let mut asm = FrameAssembler::new(48_000.0, 10.0);
        let frames = collect(&mut asm, &[]);
        assert!(frames.is_empty());
        assert_eq!(asm.frames_emitted(), 0);
    }
}
