//! 检测器配置的再导出。
//!
//! 类型本体在 `noise_core::detector_config`——[`MonitorConfig`] 要按字段
//! 持有它，而 `noise-detect` 依赖 `noise-core`，反过来放就成环了。
//!
//! 这里保留一个模块而不是把 `noise_detect::config` 直接删掉，是因为
//! `noise_detect::config::DetectorConfig` 这个路径已经散落在调用方、
//! 测试和文档里，改名只会制造一次无谓的全仓库改动。
//!
//! [`MonitorConfig`]: noise_core::MonitorConfig

pub use noise_core::{ConfigError, DetectorConfig};
