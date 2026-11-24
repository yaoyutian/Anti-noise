//! 实时回调纪律守卫。
//!
//! # 防的是什么
//!
//! 两个 cpal 回调跑在操作系统持有的实时线程上，那里**分配内存 = 掉帧**。
//! 分配会走到 malloc，malloc 可能加锁、可能触发 `mmap`、可能被别的线程
//! 持有的堆锁挡住——时长从几十微秒到几十毫秒不等，而设备缓冲只有
//! 10–30 ms。结果是音频子系统出爆音，**包括用户在听的东西**。
//!
//! 危险之处在于这类违规在引起爆音之前**完全静默**：多一个 `Vec::new()`
//! 谁也不会注意到，直到某天机器负载高的时候开始周期性爆音，那时再回头
//! 找是哪一行已经太晚了。
//!
//! # 为什么是计数而不是 panic
//!
//! 直觉上「在分配器里 panic」最解气，但它做不到，两个理由都是硬的：
//!
//! 1. **panic 自己会分配**。`panic!` 把消息装箱、准备回溯——都会再进
//!    一次分配器，于是递归。要打破递归就得在分配器里设一个「正在 panic」
//!    标志，那又是第二个隐患。
//! 2. **跨越 C 边界展开是未定义行为**。cpal 回调是从 WASAPI / CoreAudio
//!    的 C 回调里调进来的，panic 展开穿过它没有定义行为。最乐观的结果是
//!    abort，最悲观的是栈被撕成两半、程序带着半死的音频线程继续跑。
//!
//! 所以这里**只计数**，真正的报告交给非实时线程：引擎每秒读一次
//! [`take_breach`]，记 ERROR 日志并推一条 `monitor://error` 到 UI。
//! 这比 panic 更好用——监测软件要跑整夜，一个响亮的错误比一次崩溃有用。
//!
//! # 为什么标志是线程局部的
//!
//! 「正在回调里」只对本线程有意义。用全局原子的话，测试并行跑时会互相
//! 看见对方的标志——A 线程在回调里分配，B 线程的计数跟着涨，于是这个
//! 守卫在测试里永远报警、在实机上永远不报警。
//!
//! 同样重要的是**深度而不是布尔**：将来若有嵌套的回调包装（比如自己
//! 加一层限幅器），内层 `drop` 不能把外层的标志清掉。
//!
//! # 开关
//!
//! 计数分配器要 `feature = "rt-guard"` **且** `debug_assertions` 同时
//! 成立。两个条件都要，是因为任一个单独都不够：只判 `debug_assertions`
//! 会让 release 之外的任何构建都背上一次线程局部访问；只判 feature 则
//! 万一这个 feature 顺着 `--all-features` 漏进发布包，代价就落到用户
//! 头上。守卫本身（[`RtGuard`] / [`violations`]）无条件可用。

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

thread_local! {
    /// 本线程进入音频回调的深度。
    ///
    /// `const` 初始化很重要：惰性初始化要在首次访问时跑一遍初始化函数，
    /// 那可能分配——而首次访问恰恰就发生在分配器内部。
    static DEPTH: Cell<u32> = const { Cell::new(0) };

    /// 本线程在回调内发生的分配次数。
    ///
    /// 除了诊断，它还是**测试能确定断言的量**：进程级的 [`violations`]
    /// 会被并行跑的其它测试线程搅浑，而这个是干净的。
    static LOCAL_VIOLATIONS: Cell<u64> = const { Cell::new(0) };
}

/// 进程级违规总数。
static VIOLATIONS: AtomicU64 = AtomicU64::new(0);

/// 「自上次读取以来又违规了」。给非实时线程轮询用。
static BREACHED: AtomicBool = AtomicBool::new(false);

/// 进入/离开一段不允许分配的代码。
///
/// 在 cpal 回调的第一行 `let _rt = RtGuard::enter();`，回调结束时自动
/// 离开。**必须绑到一个有名字的变量上**（`_rt` 而不是 `_`）——`let _ =`
/// 会立刻析构，守卫等于没进。这是个很容易犯的错，所以这里没有别的用法。
pub struct RtGuard {
    /// 私有字段：外部无法构造，只能通过 [`RtGuard::enter`]。
    _private: (),
}

impl RtGuard {
    /// 标记「从这里开始到 drop 为止是本线程的实时区」。
    #[inline]
    pub fn enter() -> Self {
        DEPTH.with(|d| d.set(d.get().saturating_add(1)));
        Self { _private: () }
    }
}

impl Drop for RtGuard {
    #[inline]
    fn drop(&mut self) {
        DEPTH.with(|d| {
            // saturating_sub 而不是 -1：`RtGuard` 是 `!Copy` 的，配平由
            // 类型系统保证，但真出现下溢时我们想要的是「守卫失效」而不是
            // 「整数溢出的 debug panic」——后者会从 drop 里炸出来，
            // 而 drop 可能正好在回调里。
            d.set(d.get().saturating_sub(1));
        });
    }
}

/// 本线程此刻是否在实时区内。
///
/// 分配器每次分配都会问一次这个问题，所以它必须廉价：`const` 初始化的
/// 线程局部访问是一次相对寻址，不加锁、不分配。
#[inline]
pub fn in_callback() -> bool {
    DEPTH.with(|d| d.get() > 0)
}

/// 记一次违规。分配器内部调用。
///
/// 跟着计数分配器一起条件编译：没有分配器的时候没有任何调用方，
/// 留着一个永远不被调用的函数只会让 `dead_code` 永远亮着——而一条
/// 常年亮着的警告真正的代价是，下一条新警告出现时没人会注意到。
#[cfg(all(debug_assertions, feature = "rt-guard"))]
#[inline]
fn note_violation() {
    LOCAL_VIOLATIONS.with(|c| c.set(c.get().saturating_add(1)));
    VIOLATIONS.fetch_add(1, Ordering::Relaxed);
    BREACHED.store(true, Ordering::Relaxed);
}

/// 本线程累计的违规次数。
pub fn local_violations() -> u64 {
    LOCAL_VIOLATIONS.with(Cell::get)
}

/// 进程累计的违规次数。
pub fn violations() -> u64 {
    VIOLATIONS.load(Ordering::Relaxed)
}

/// 读走「又违规了」这个边沿信号。
///
/// 引擎线程每秒调一次：返回 `true` 说明自上次调用以来实时回调里发生过
/// 分配，该记日志、该推 UI 告警了。**返回 `bool` 而不是计数**，是因为
/// 调用方要的是「要不要报一次」，不是「报几次」。
pub fn take_breach() -> bool {
    BREACHED.swap(false, Ordering::Relaxed)
}

/// 把当前线程的计数清零。测试用。
#[cfg(test)]
pub fn reset_local() {
    LOCAL_VIOLATIONS.with(|c| c.set(0));
}

/// 计数分配器。
///
/// 只包装 `System`，在每次真正分配前问一句「现在在回调里吗」。`dealloc`
/// 不检查：**释放不是分配**，它不该触发告警（而且回调结束时释放临时对象
/// 是合法且常见的做法）。
#[cfg(all(debug_assertions, feature = "rt-guard"))]
mod counting {
    use super::{in_callback, note_violation};
    use std::alloc::{GlobalAlloc, Layout, System};

    pub struct CountingAlloc;

    // SAFETY: 每个方法都只是加一次计数后原样转发给 `System`，
    // 不改变指针的所有权、对齐或大小语义，因此 `System` 满足的
    // `GlobalAlloc` 契约在这里同样成立。
    unsafe impl GlobalAlloc for CountingAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if in_callback() {
                note_violation();
            }
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if in_callback() {
                note_violation();
            }
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if in_callback() {
                note_violation();
            }
            unsafe { System.realloc(ptr, layout, new_size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }
}

#[cfg(all(debug_assertions, feature = "rt-guard"))]
#[global_allocator]
static RT_ALLOCATOR: counting::CountingAlloc = counting::CountingAlloc;

/// 编译期就把这条守卫的存在与否暴露出来，免得「我明明开了这个 feature
/// 怎么什么都没报」变成一个需要读 Cargo.toml 才能回答的问题。
#[cfg(all(debug_assertions, feature = "rt-guard"))]
pub const ENABLED: bool = true;

/// 见 [`ENABLED`]。
#[cfg(not(all(debug_assertions, feature = "rt-guard")))]
pub const ENABLED: bool = false;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_is_clear_outside_and_set_inside() {
        assert!(!in_callback(), "正常代码路径不该被当成实时区");
        {
            let _rt = RtGuard::enter();
            assert!(in_callback());
        }
        assert!(!in_callback(), "离开后必须复位");
    }

    /// 嵌套时内层离开不能把外层的标志清掉。将来加一层回调内的包装
    /// 就会用到这个行为，而写错了的表现是「守卫时灵时不灵」。
    #[test]
    fn nested_guards_unwind_in_order() {
        let outer = RtGuard::enter();
        let inner = RtGuard::enter();
        drop(inner);
        assert!(in_callback(), "内层退出后外层仍应在实时区内");
        drop(outer);
        assert!(!in_callback());
    }

    /// 标志是**线程局部**的：一个线程在回调里，不该让另一个线程的
    /// 分配被记成违规。搞错这一点的后果是守卫在测试里永远报警、
    /// 在实机上永远不报警——恰好完全反了。
    #[test]
    fn the_flag_does_not_leak_across_threads() {
        let _rt = RtGuard::enter();
        let observed = std::thread::spawn(in_callback).join().unwrap();
        assert!(!observed, "另一个线程不该看见本线程的实时标志");
        assert!(in_callback(), "本线程自己仍然在实时区内");
    }

    /// 真正的端到端判据：在守卫内部分配，计数必须动。
    ///
    /// 只在计数分配器真的挂上时才成立（`--all-features` 或显式开
    /// `rt-guard` 且是 debug 构建）。没挂上时这个测试会退化成一条
    /// 无意义的断言，所以干脆整个不编译。
    #[cfg(all(debug_assertions, feature = "rt-guard"))]
    #[test]
    fn allocation_inside_a_guard_is_counted() {
        reset_local();
        {
            let _rt = RtGuard::enter();
            // 这个 `Vec` 必须在守卫内构造并推入——`Vec::new()` 本身
            // 不分配，第一次 push 才分配。
            let mut v: Vec<u8> = Vec::new();
            v.push(1);
            v.extend_from_slice(&[2, 3, 4]);
            std::hint::black_box(&v);
        }
        assert!(
            local_violations() >= 1,
            "回调内的分配没有被计数——计数分配器没挂上？"
        );
    }

    /// 守卫外分配不该被算进去。否则计数器只是在数全程序的分配，
    /// 那个数字没有任何诊断价值。
    #[cfg(all(debug_assertions, feature = "rt-guard"))]
    #[test]
    fn allocation_outside_a_guard_is_not_counted() {
        reset_local();
        let mut v: Vec<u8> = Vec::new();
        v.push(1);
        std::hint::black_box(&v);
        assert_eq!(local_violations(), 0, "守卫外的分配被误记了");
    }

    #[test]
    fn enabled_flag_matches_the_allocator() {
        // 这两者必须一致：`ENABLED` 存在的唯一意义就是让调用方能问
        // 「守卫到底挂上了没有」，它说谎比没有它更糟。
        #[cfg(all(debug_assertions, feature = "rt-guard"))]
        assert!(ENABLED);
        #[cfg(not(all(debug_assertions, feature = "rt-guard")))]
        assert!(!ENABLED);
    }
}
