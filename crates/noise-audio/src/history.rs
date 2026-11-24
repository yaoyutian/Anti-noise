//! pre-roll 环形历史。
//!
//! # 它解决的是什么
//!
//! 事件是在**发生之后**才被确认的：上升沿要等一帧，冲击要再等一帧确认
//! 衰减（见 `noise_detect::sm`）。等检测器说「这是一次冲击」时，声音已经
//! 过去了几十毫秒。如果录音从那一刻开始，**每段证据都会从撞击之后开始**，
//! 而那正是整件事里唯一有价值的几十毫秒。
//!
//! 所以录音必须先有一份「刚刚过去的 N 秒」。
//!
//! # 为什么是环形缓冲而不是「先开始录、事后裁剪」
//!
//! 后者要求磁盘一直在写，然后不断删掉前面——一整夜的持续写入换来的是
//! 一个只偶尔用得上的文件，而且磁盘 I/O 的错误处理路径变成了主路径。
//! 环形缓冲把这件事放在内存里，代价是 15 秒的固定开销。
//!
//! # 为什么必须样本精确
//!
//! 触发链是**逐帧**推进的，帧时基与样本时基之间差着一个帧对齐的余数。
//! 「从事件开始往回数 15 秒」和「从这一帧的起点往回数 15 秒」会差最多
//! 一帧。这本身无关紧要——**但同一个事件在 WAV 和 `events.jsonl` 里的
//! 时间戳必须对得上**，而事后拿 Audacity 去核对时，任何系统性偏移都会
//! 变成一次「这个软件的时间戳是不是不准」的怀疑。
//!
//! 所以这里是**绝对样本索引**：每个样本有一个从会话开始起算、单调递增的
//! 编号，一切裁剪都按编号算，不按「往回数多少个」。环形缓冲只负责
//! 「编号 → 内存位置」的映射。
//!
//! # 容量上限
//!
//! 配置里的 `pre_roll_ms` 是用户可编辑的。一个手滑输成 `1500000` 的
//! 配置如果照字面执行，会在按「开始监测」的瞬间申请几个 GB。
//! [`MAX_HISTORY_SAMPLES`] 把它挡住，并且 [`AudioHistory::is_clamped`]
//! 让调用方能**说出来**——静默截断会让用户以为自己在看 25 分钟的历史。

/// 历史缓冲的样本数上限。
///
/// 5_760_000 个 f32 是 22 MB，也就是 192 kHz 下的 30 秒 / 48 kHz 下的
/// 120 秒。够长到覆盖任何合理的 pre-roll（默认 15 秒），短到不至于
/// 让一个写错的配置把内存吃掉。
pub const MAX_HISTORY_SAMPLES: usize = 5_760_000;

/// 过去 N 秒音频的环形缓冲。
#[derive(Debug, Clone)]
pub struct AudioHistory {
    /// 后备存储。长度是 `capacity.max(1)`——见 [`Self::capacity`]。
    buf: Vec<f32>,
    /// **逻辑**容量。可以为 0（调用方算错了）。
    ///
    /// 单独存一个字段而不是每次问 `buf.len()`：容量 0 时 `buf` 里仍然
    /// 有一个占位元素（取模需要一个非零除数），于是 `buf.len()` 会
    /// 谎报成 1。让「能存多少」和「分配了多少」是两个字段，
    /// 这里就没有任何需要靠注释维持的隐含约定。
    capacity: usize,
    /// 下一个写入位置。恒 `< capacity`（`capacity > 0` 时）。
    head: usize,
    /// 当前有效样本数（≤ `capacity`）。
    len: usize,
    /// 配置要求的容量超过了上限而被截断。
    clamped: bool,
    /// 从建立以来累计写入的样本数。**绝对索引的上界**（不含）。
    ///
    /// `u64` 而不是 `usize`：48 kHz 下 `usize` 要跑几百万年才溢出，
    /// 但磁盘上的样本索引、WAV 里的帧号、以及将来可能的多段拼接
    /// 都用得上这个量级，而换类型的代价是改一遍所有签名。
    written: u64,
}

impl AudioHistory {
    /// 建一个能装 `capacity_samples` 个样本的环。
    ///
    /// 容量为 0 时退化成「不保存任何历史」而不是 panic ——
    /// 这个构造函数跑在开始监测的路径上，用户点了「开始」之后崩掉
    /// 是最糟的失败方式。返回的对象一切正常，只是快照永远是空的。
    pub fn new(capacity_samples: usize) -> Self {
        let clamped = capacity_samples > MAX_HISTORY_SAMPLES;
        let capacity = capacity_samples.min(MAX_HISTORY_SAMPLES);
        Self {
            // 至少分配 1 个槽位：取模需要一个非零的除数。容量 0 时
            // `head` 永远停在 0，那一个槽位永远不会被写。
            buf: vec![0.0; capacity.max(1)],
            capacity,
            head: 0,
            len: 0,
            clamped,
            written: 0,
        }
    }

    /// 按秒数建，这是实际调用方式。
    pub fn with_seconds(sample_rate: f64, seconds: f64) -> Self {
        let n = if sample_rate.is_finite() && seconds.is_finite() && seconds > 0.0 {
            (sample_rate * seconds).round() as usize
        } else {
            0
        };
        Self::new(n)
    }

    /// 装得下多少个样本。
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 当前存了多少个。
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 配置要求的容量被上限截断了。**必须报给用户**，见模块文档。
    pub fn is_clamped(&self) -> bool {
        self.clamped
    }

    /// 现在能取到的最早那个样本的绝对索引。
    pub fn oldest_index(&self) -> u64 {
        self.written - self.len as u64
    }

    /// 下一个将被写入的样本的绝对索引（也就是已写入的总数）。
    pub fn next_index(&self) -> u64 {
        self.written
    }

    /// 追加一段样本，覆盖最旧的。
    pub fn push(&mut self, x: &[f32]) {
        // 绝对索引**先**推进：它是时间轴，与留不留得住数据无关。
        // 容量为 0 时它是唯一还在动的东西，而它必须动——
        // 停了的话「事件在样本几」会随着时间漂移。
        self.written += x.len() as u64;
        if self.capacity == 0 {
            return;
        }
        for &v in x {
            self.buf[self.head] = v;
            self.head += 1;
            if self.head == self.capacity {
                self.head = 0;
            }
            if self.len < self.capacity {
                self.len += 1;
            }
        }
    }

    /// 把 `[from, to)` 这一段绝对样本拷进 `out`，返回拷贝了几个。
    ///
    /// 超出已有范围的部分被**裁剪**而不是补零：补零会让 WAV 开头出现
    /// 一段静音，而事后看到那段静音的人会以为麦克风当时断了。
    /// 裁剪的代价是事件刚触发时录音短一点，那是实话。
    pub fn copy_range(&self, from: u64, to: u64, out: &mut Vec<f32>) -> usize {
        out.clear();
        let oldest = self.oldest_index();
        let newest = self.written;
        let from = from.max(oldest).min(newest);
        let to = to.max(from).min(newest);
        let n = (to - from) as usize;
        if n == 0 {
            return 0;
        }
        // `from >= oldest` 保证了 `newest - from <= len <= capacity`，
        // 所以这一次减法不会下溢，取模的除数也是正的（`n > 0` 蕴含
        // `capacity > 0`）。
        let back = (newest - from) as usize;
        let start = (self.head + self.capacity - back) % self.capacity;
        self.copy_from_pos(start, n, out);
        n
    }

    /// 取事件开始前的 `want` 个样本，`end_exclusive` 是事件起点的绝对索引。
    ///
    /// 这是**录音路径唯一需要的入口**，写成这样而不是
    /// 「从头取 `len()` 个」，是因为事件起点往往不是最后一个样本——
    /// 冲击确认要晚两帧，而这期间又有新样本进来了。取到末尾的话，
    /// 录音里会凭空多出 20 ms 的「未来」，而 pre-roll 反而少了 20 ms。
    pub fn copy_before(&self, end_exclusive: u64, want: usize, out: &mut Vec<f32>) -> usize {
        let from = end_exclusive.saturating_sub(want as u64);
        self.copy_range(from, end_exclusive, out)
    }

    /// 取全部有效样本，按时间顺序（最旧在前）。
    pub fn snapshot_into(&self, out: &mut Vec<f32>) {
        self.copy_range(self.oldest_index(), self.written, out);
    }

    /// 清空内容但**保留容量**，并让绝对索引继续前进。
    ///
    /// 绝对索引**不归零**：它是会话内的时间轴，重启一次监测就把它归零
    /// 会让重启前后的事件时间戳互相重叠。要归零的话请重建整个对象。
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// 从缓冲位置 `pos` 起拷 `n` 个样本，必要时跨过绕回点。
    ///
    /// 越过 `capacity` 的部分来自 `buf[0..]`——`buf` 可能比 `capacity`
    /// 长一个占位元素，而 `n - first < capacity` 保证了第二段永远
    /// 不会碰到它。
    fn copy_from_pos(&self, pos: usize, n: usize, out: &mut Vec<f32>) {
        let first = (self.capacity - pos).min(n);
        out.extend_from_slice(&self.buf[pos..pos + first]);
        if n > first {
            out.extend_from_slice(&self.buf[..n - first]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 顺序：最旧的在最前面。环形缓冲最经典的 bug 就是绕回之后
    /// 把两段的先后写反，而写反的表现是录音里前后两截倒过来——
    /// 听感上像是「咔哒一声在声音之前」，很容易被当成拾音问题。
    #[test]
    fn wraparound_keeps_chronological_order() {
        let mut h = AudioHistory::new(8);
        h.push(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]); // 未满
        let mut out = Vec::new();
        h.snapshot_into(&mut out);
        assert_eq!(out, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);

        h.push(&[6.0, 7.0, 8.0, 9.0]); // 绕回，6.0 8.0 被覆盖
        h.snapshot_into(&mut out);
        assert_eq!(out, vec![2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
    }

    /// 一次推入超过容量的数据：只留**最新**的那些，而且顺序不乱。
    ///
    /// 环形缓冲在这种情形下最容易错——直觉上「一次推 20 个进 8 格」
    /// 会写成先把前面覆盖一遍再把整段拷进去，结果是新旧交错。
    #[test]
    fn a_chunk_larger_than_capacity_keeps_its_tail_in_order() {
        let mut h = AudioHistory::new(4);
        h.push(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);

        let mut out = Vec::new();
        h.snapshot_into(&mut out);
        assert_eq!(out, vec![6.0, 7.0, 8.0, 9.0]);
        assert_eq!(h.len(), 4);
    }

    /// 绝对索引与「往回数 N 个」是**两件事**，这条测试钉住它们的区别。
    #[test]
    fn absolute_indices_survive_wraparound() {
        let mut h = AudioHistory::new(4);
        h.push(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(h.next_index(), 6);
        assert_eq!(h.oldest_index(), 2, "只留得住索引 2..6");
        assert_eq!(h.len(), 4);

        // 取绝对区间 [3, 6)：值就是索引本身
        let mut out = Vec::new();
        assert_eq!(h.copy_range(3, 6, &mut out), 3);
        assert_eq!(out, vec![3.0, 4.0, 5.0]);

        // 越界的左端被裁到 oldest，不是补零
        assert_eq!(h.copy_range(0, 4, &mut out), 2);
        assert_eq!(out, vec![2.0, 3.0]);
    }

    /// 事件起点往往不是最后一个样本——冲击确认要晚两帧。
    /// `copy_before` 必须取到「起点之前」而不是「末尾之前」。
    #[test]
    fn copy_before_ends_at_the_given_index_not_at_the_tail() {
        let mut h = AudioHistory::new(16);
        h.push(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);

        let mut out = Vec::new();
        // 事件起点在索引 5，要 3 个样本 → [2, 5)
        assert_eq!(h.copy_before(5, 3, &mut out), 3);
        assert_eq!(out, vec![2.0, 3.0, 4.0]);
        assert_eq!(
            out.last(),
            Some(&4.0),
            "绝不能把起点之后的样本也算进来——那是「未来」"
        );
    }

    /// 历史还没攒够时，取到的就是现有这些，不补零。
    #[test]
    fn an_unfilled_history_returns_what_it_has() {
        let mut h = AudioHistory::new(100);
        h.push(&[1.0, 2.0, 3.0]);
        let mut out = Vec::new();
        assert_eq!(h.copy_before(3, 50, &mut out), 3);
        assert_eq!(out, vec![1.0, 2.0, 3.0]);
        assert_eq!(h.oldest_index(), 0);
    }

    /// 容量 0 是算错了（采样率是 0、配置写错），不能 panic，
    /// 也不能让绝对索引停下——索引停了，事件时间戳就会互相重叠。
    #[test]
    fn zero_capacity_is_inert_but_keeps_the_index_advancing() {
        let mut h = AudioHistory::new(0);
        h.push(&[1.0, 2.0, 3.0]);
        let mut out = Vec::new();
        h.snapshot_into(&mut out);
        assert!(out.is_empty());
        assert!(h.is_empty());
        assert_eq!(h.next_index(), 3, "绝对索引必须照常前进");

        h.push(&[4.0, 5.0]);
        assert_eq!(h.next_index(), 5);
        assert_eq!(h.oldest_index(), 5, "没有任何有效样本");
    }

    /// 一个手滑写成 `pre_roll_ms = 1500000` 的配置不能真的去申请内存。
    #[test]
    fn an_absurd_capacity_is_clamped_and_reported() {
        let h = AudioHistory::new(usize::MAX / 2);
        assert_eq!(h.capacity(), MAX_HISTORY_SAMPLES);
        assert!(h.is_clamped(), "截断必须能被报出来");

        let h = AudioHistory::new(48_000 * 15);
        assert!(!h.is_clamped(), "正常的 15 秒不该被标记");
    }

    /// `with_seconds` 遇到非法输入（采样率 0、时长负数或 NaN）
    /// 要退化成空历史而不是 panic 或申请一块天文数字的内存。
    #[test]
    fn with_seconds_rejects_nonsense_input() {
        assert_eq!(AudioHistory::with_seconds(0.0, 15.0).capacity(), 0);
        assert_eq!(AudioHistory::with_seconds(48_000.0, -1.0).capacity(), 0);
        assert_eq!(AudioHistory::with_seconds(f64::NAN, 15.0).capacity(), 0);
        assert_eq!(AudioHistory::with_seconds(48_000.0, f64::INFINITY).capacity(), 0);
        assert_eq!(
            AudioHistory::with_seconds(48_000.0, 15.0).capacity(),
            720_000,
            "48 kHz × 15 s"
        );
    }

    /// 连续推入大量数据之后，环里的内容必须和「最后 capacity 个」完全一致。
    /// 这条穷举各种推入切分方式，专抓绕回时的边界错误。
    #[test]
    fn content_always_equals_the_last_capacity_samples() {
        let mut h = AudioHistory::new(7);
        let mut all: Vec<f32> = Vec::new();
        let mut n = 0.0f32;

        // 各种切分：1、2、3、5、7、9、13 —— 覆盖比容量大和小于容量
        for chunk in [1usize, 2, 3, 5, 7, 9, 13].iter().cycle().take(12) {
            let batch: Vec<f32> = (0..*chunk).map(|_| {
                n += 1.0;
                n
            }).collect();
            all.extend_from_slice(&batch);
            h.push(&batch);

            let expected: Vec<f32> = all[all.len().saturating_sub(7)..].to_vec();
            let mut out = Vec::new();
            h.snapshot_into(&mut out);
            assert_eq!(out, expected, "推入 {chunk} 个之后对不上");
        }
    }

    /// `clear` 清内容但**不归零绝对索引**：会话内的时间轴不能被一次
    /// 清空打断，否则重启前后的事件时间戳会互相重叠。
    #[test]
    fn clear_keeps_the_absolute_timeline() {
        let mut h = AudioHistory::new(8);
        h.push(&[1.0; 10]);
        assert_eq!(h.next_index(), 10);

        h.clear();
        assert!(h.is_empty());
        assert_eq!(h.next_index(), 10, "绝对索引不该被清零");
        assert_eq!(h.oldest_index(), 10);

        h.push(&[2.0; 3]);
        assert_eq!(h.next_index(), 13);
        let mut out = Vec::new();
        h.snapshot_into(&mut out);
        assert_eq!(out, vec![2.0, 2.0, 2.0]);
    }
}
