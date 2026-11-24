//! 领域词汇与配置类型。
//!
//! 本 crate 不依赖任何音频库，因此可以被仓库中任意位置安全引用。
//!
//! # 单位约定
//!
//! **内部一律使用线性量（f64），dB 只出现在边界上。** 声压级、能量、
//! 滤波系数在计算过程中保持线性；只有在写入事件记录或送往 UI 时才转成 dB。
//! 混用两者是这个领域最常见的错误来源，[`units`] 里的 newtype 存在的意义
//! 就是让这种错误无法通过编译。

pub mod config;
pub mod detector_config;
pub mod error;
pub mod event;
pub mod ids;
pub mod units;

pub use config::{
    CalibrationConfig, MonitorConfig, PlaybackConfig, RecordingConfig, RetentionPolicy,
};
pub use detector_config::{ConfigError, DetectorConfig};
pub use error::CoreError;
pub use event::{EventFlags, EventKind};
pub use ids::{EventId, ResponseId, SeqCounter, SessionId};
pub use units::{DbA, Dbfs, DbSpl, Hertz, Milliseconds, Samples};

/// 参考声压 20 µPa —— dB SPL 的零点。
pub const P0: f64 = 20e-6;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_pressure_matches_iec_definition() {
        assert_eq!(P0, 2e-5);
    }
}
