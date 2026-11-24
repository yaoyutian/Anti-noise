//! 触发检测：自适应底噪、迟滞阈值、冲击状态机、半双工门控。
//!
//! # 这个 crate 不链接 `cpal`
//!
//! 不是风格偏好，是**编译期强制**的实时约束。实时音频回调里不能有分配、
//! 加锁、日志、阻塞——而这类违规在引起爆音之前是完全静默的。把状态机
//! 放在一个连 `cpal` 都链接不到的 crate 里，它就不可能偶然从回调里
//! 被调用；线程与锁的边界因此由 Cargo 而不是代码评审来保证。
//!
//! # 分层
//!
//! ```text
//!   noise-dsp   滤波器、计权、八度带、指标      （纯函数，无状态机）
//!        │
//!        ▼  FrameFeatures
//!   noise-detect                                 （本 crate）
//!        ├── floor       自适应底噪，升快降慢
//!        ├── hysteresis  门限推导 + 实测耦合校正
//!        ├── gate        半双工门控（L1–L4）
//!        └── sm          触发状态机
//! ```
//!
//! # 两条路径在这里分岔
//!
//! * **触发路径**（本 crate 的 `process`）—— 20–200 Hz、未计权、快、可丢弃
//! * **取证路径**（调用方）—— 宽带、A 计权、校准、必须可信
//!
//! A 计权在 63 Hz 处衰减 26 dB，而脚步能量正落在那里。**触发链绝不过
//! A 计权**，否则等于先把要检测的信号扔掉。`FrameFeatures` 里根本没有
//! A 计权字段，就是为了让这条纪律无法被无意违反。

pub mod config;
pub mod floor;
pub mod frame;
pub mod gate;
pub mod hysteresis;
pub mod sm;

/// 离线特征提取与 replay 工具链。
///
/// 无条件编译，**不是**测试脚手架：场景测试、`examples/replay.rs`、
/// 以及 `src-tauri` 的 `--replay` 子命令走的都是这里。把它们锁在
/// `#[cfg(test)]` 后面会让集成测试拿不到它（集成测试看不到 lib 的
/// `cfg(test)`），而各自复制一份就会立刻退化成两条会分叉的路径——
/// 那正是「用录音调参数」这个工作流最不能承受的。
pub mod offline;

pub use config::{ConfigError, DetectorConfig};
pub use floor::{AdaptiveNoiseFloor, DB_FLOOR};
pub use frame::{FrameAssembler, FrameStats};
pub use gate::{GateState, HalfDuplexGate, ResponseRefusal};
pub use hysteresis::Thresholds;
pub use sm::{
    Action, Detector, DetectorCounters, DetectorState, EndedEvent, Suppression,
};
