//! 音频引擎。
//!
//! **本仓库中唯一允许链接 `cpal` 的 crate。** 其余 crate 不依赖 cpal，
//! 因而在编译期就不可能违反实时约束——这条边界由 Cargo 而不是代码评审
//! 来维持，理由见 `noise-detect` 的 crate 文档。
//!
//! # 实时线程纪律
//!
//! 两个 cpal 回调运行在操作系统持有的实时线程上，其中**绝对禁止**：
//! 分配内存、加锁、写日志、阻塞、发送 Tauri 事件。回调唯一允许做的事
//! 是把样本转换后推入无锁环。
//!
//! 分析线程、写入线程、引擎线程、命令线程都是**普通线程**，可以用锁和分配。
//!
//! 这条纪律不能靠自觉。开 `rt-guard` feature 做 debug 构建时，
//! [`rtguard`] 会挂上一个计数全局分配器，任何在回调内的分配都会被记账，
//! 再由引擎线程报给 UI。
//!
//! # 模块分层
//!
//! ```text
//! 实时回调可用的（无分配、无锁）
//!     convert    12 种采样格式 → 单声道 f32，含 NaN 清洗
//!     ring       无锁 SPSC 环 + 丢弃计数
//!     rtguard    回调边界守卫（debug 构建）
//!     output     回放回调：增益斜坡、耗尽静音
//!
//! 非实时线程用的
//!     device     枚举与「实际达成」的参数协商
//!     history    pre-roll 环形历史（绝对样本索引）
//!     response   反击波形生成
//!     status     原子状态 + 电平快照
//!     input      输入流与分块降混
//!     analysis   分析线程：拥有 DSP、检测器、历史
//!     engine     流的归属、半双工监督、退避重启
//! ```
//!
//! # 两条路径在这里分开
//!
//! * **触发路径**：`analysis` 里的窄带、未计权、可丢弃的那一条
//! * **取证路径**：同一条线程上的宽带、A 计权、必须可信的那一条
//!
//! **滤波后的触发信号绝不写进录音。** 这条纪律的物理理由见
//! `noise-dsp::weighting`——A 计权在 63 Hz 处衰减 26 dB，而脚步能量
//! 正落在那里。

pub mod convert;
pub mod device;
pub mod history;
pub mod input;
pub mod output;
pub mod response;
pub mod ring;
pub mod rtguard;
pub mod status;

pub use convert::{Downmixer, DEFAULT_CAPACITY_FRAMES};
pub use device::{
    device_name, enumerate, DeviceCatalog, DeviceInfo, ErrorCounters, ErrorSink, FormatRange,
    Negotiated, StreamInfo, MIN_SAMPLE_RATE,
};
pub use history::AudioHistory;
pub use input::{build_input_stream, pending_samples, InputBuild};
pub use output::{
    build_output_stream, db_to_linear, linear_to_db, GainPlan, OutputBuild, PlaybackControl,
    PlaybackVoice, FADE_OUT_S, MAX_GAIN_DB, MIN_GAIN_DB,
};
pub use response::{build_thump, Thump, ThumpSpec, PEAK_DBFS};
pub use ring::{AudioConsumer, AudioProducer};
pub use status::{
    describe_degraded, degraded, LevelSnapshot, MonitorStatus, RunState, SharedLevel,
    SharedStatus, StatusSnapshot,
};
