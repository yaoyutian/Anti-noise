//! 无锁单生产者单消费者环，以及它的丢弃计数。
//!
//! # 为什么环满时丢弃而不是等待
//!
//! 生产端是实时音频回调。**「等消费者腾出位置」在这里等于让音频线程
//! 阻塞**，而音频线程阻塞的后果不是丢一块数据，是整台机器的音频子系统
//! 出爆音——包括用户在听的东西。
//!
//! 两秒的容量（48 kHz 下 96000 个样本）意味着消费者要卡住整整两秒才会
//! 开始丢。真发生这种事，丢几块样本是**已经发生的故障的表征**，不是
//! 故障本身；把它计数报出来，比让它阻塞然后掩盖一切有用得多。
//!
//! # 丢弃计数为什么在 `Producer` 上
//!
//! 因为它只能由生产端递增——那是唯一知道「这块没塞进去」的地方。
//! 放在共享的 `Arc` 里就变成了两个线程的竞争，而这里连原子都不需要
//! 跨线程共享：UI 通过 [`crate::status`] 的快照读它。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rtrb::{Consumer, Producer, RingBuffer};

/// 生产端：实时回调持有。
pub struct AudioProducer {
    inner: Producer<f32>,
    dropped: Arc<AtomicU64>,
    /// 累计成功推送的样本数。样本精确，用于与录音对时。
    pushed: u64,
}

/// 消费端：分析线程持有。
pub struct AudioConsumer {
    inner: Consumer<f32>,
    dropped: Arc<AtomicU64>,
}

impl AudioProducer {
    /// 建一个容量为 `capacity_samples` 的环。
    ///
    /// 容量会被 `rtrb` 向上取整到 2 的幂并**多要一个槽位**（环的经典
    /// 实现要用一个空槽区分「满」和「空」），所以实际能存的是容量减一。
    /// 这里不做补偿——两秒容量差一个样本无关紧要，而假装容量精确会
    /// 让「为什么少了一个」变成一个将来必须重新调查的问题。
    pub fn new(capacity_samples: usize) -> (Self, AudioConsumer) {
        let (inner, consumer) = RingBuffer::new(capacity_samples.max(2));
        let dropped = Arc::new(AtomicU64::new(0));
        (
            Self {
                inner,
                dropped: Arc::clone(&dropped),
                pushed: 0,
            },
            AudioConsumer {
                inner: consumer,
                dropped,
            },
        )
    }

    /// 尽量推入，返回**没能推入的样本数**。
    ///
    /// 不用 `Result` 是刻意的：调用方（实时回调）对「没推完」唯一正确的
    /// 反应就是记个数继续跑，返回 `Err` 只会诱使某个调用点写 `?` 然后
    /// 把错误往上抛——而回调里没有「上」可抛。
    #[inline]
    pub fn push_partial(&mut self, data: &[f32]) -> usize {
        let (accepted, _) = self.inner.push_partial_slice(data);
        let n = accepted.len();
        self.pushed += n as u64;
        let dropped = data.len() - n;
        if dropped > 0 {
            self.dropped.fetch_add(dropped as u64, Ordering::Relaxed);
        }
        dropped
    }

    /// 累计推入的样本数。实时回调里读本地字段，不是原子。
    #[inline]
    pub fn pushed_samples(&self) -> u64 {
        self.pushed
    }

    /// 还能推多少个样本。给「这块塞不塞得下」的预判用。
    #[inline]
    pub fn free_slots(&self) -> usize {
        self.inner.slots()
    }
}

impl AudioConsumer {
    /// 最多取 `out.len()` 个样本，返回实际取到的个数。
    ///
    /// 用 `pop_partial_slice` 而不是手工 `read_chunk` + `commit`：
    /// **忘了 commit 的后果是静默的**——数据留在环里被反复读到，
    /// 表现为周期性的「回声」，而不是一个报错。让库来管这件事。
    pub fn pop_into(&mut self, out: &mut [f32]) -> usize {
        let (filled, _) = self.inner.pop_partial_slice(out);
        filled.len()
    }

    /// 环里现在有多少个样本可读。
    pub fn len(&self) -> usize {
        self.inner.slots()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// 从建立到现在，生产端一共丢了多少样本。
    pub fn dropped_samples(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_order_and_values() {
        let (mut p, mut c) = AudioProducer::new(64);
        let data: Vec<f32> = (0..40).map(|i| i as f32 * 0.5).collect();
        assert_eq!(p.push_partial(&data), 0);

        let mut out = vec![0.0f32; 40];
        assert_eq!(c.pop_into(&mut out), 40);
        assert_eq!(out, data);
    }

    /// 环满时丢**新**数据，不覆盖旧数据，并且如实计数。
    ///
    /// 丢新的而不是丢旧的：分析线程看的是「最近发生了什么」，
    /// 覆盖旧数据会让事件时间戳错乱。
    #[test]
    fn overflow_drops_the_newest_and_counts_it() {
        let (mut p, mut c) = AudioProducer::new(8);
        // rtrb 会向上取整到 2 的幂，实际容量看 free_slots
        let cap = p.free_slots();
        let big: Vec<f32> = vec![1.0; cap + 5];

        let dropped = p.push_partial(&big);
        assert_eq!(dropped, 5, "应只丢掉超出的 5 个");
        assert_eq!(c.dropped_samples(), 5);

        let mut out = vec![0.0f32; cap];
        assert_eq!(c.pop_into(&mut out), cap);
        assert!(out.iter().all(|v| *v == 1.0), "留下的应是最先推入的那批");
    }

    #[test]
    fn empty_ring_yields_nothing() {
        let (_p, mut c) = AudioProducer::new(64);
        let mut out = vec![0.0f32; 16];
        assert_eq!(c.pop_into(&mut out), 0);
        assert!(c.is_empty());
    }

    /// 容量为 0 或 1 是调用方算错了（比如采样率是 0），不能 panic——
    /// 这个构造函数跑在开始监测的路径上，不是实时路径，但也不该在
    /// 用户点了「开始」之后崩。
    ///
    /// 但「不崩」还不够。**一个容量为 0、`free_slots()` 永远返回 0 的环
    /// 是比 panic 更难查的故障**：程序照常跑，仪表永远显示 −∞，而所有
    /// 计数器都是 0。所以要断言它确实还能装下东西。
    #[test]
    fn degenerate_capacity_does_not_panic() {
        let (mut p, mut c) = AudioProducer::new(0);

        // 钳位到 2 之后 rtrb 给出它能给的最小环，但那个环是**活的**。
        let cap = p.free_slots();
        assert!(cap >= 1, "容量 0 被钳位后仍应可写，实得容量 {cap}");

        // 退化容量下丢样本可以接受，**悄悄丢**不可以：超出的部分
        // 必须一个不少地进计数器。
        let big = vec![1.0f32; cap + 3];
        assert_eq!(p.push_partial(&big), 3, "应只丢掉超出的 3 个");
        assert_eq!(c.dropped_samples(), 3);

        let mut out = vec![0.0f32; cap];
        assert_eq!(c.pop_into(&mut out), cap, "装进去的应能原样读出来");
    }

    #[test]
    fn pushed_sample_count_is_exact() {
        let (mut p, _c) = AudioProducer::new(1024);
        p.push_partial(&[0.0; 100]);
        p.push_partial(&[0.0; 23]);
        assert_eq!(p.pushed_samples(), 123);
    }
}
