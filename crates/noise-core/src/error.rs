//! 领域错误。
//!
//! # 为什么错误类型要分层
//!
//! `noise-core` 的错误要能被 `noise-store`、`noise-audio`、Tauri 命令层
//! 各自包装，所以它只描述**领域层面**出了什么事，不描述哪一层发现的。
//! 「打不开文件」是存储层的事，「配置不合法」是领域的事——把前者塞进
//! 这里会迫使 `noise-core` 知道文件系统的存在。

use thiserror::Error;

/// `noise-core` 能报出的错误。
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    /// 配置不合法。消息面向**用户**，必须是可读的。
    ///
    /// 这条会原样出现在 UI 上，所以不能是 `Debug` 输出，也不能是英文
    /// 内部标识符。
    #[error("配置不合法：{0}")]
    InvalidConfig(String),

    /// 引用了不存在的设备。
    #[error("找不到音频设备：{0}")]
    DeviceNotFound(String),

    /// 设备存在但无法以请求的参数打开。
    ///
    /// 与 `DeviceNotFound` 分开：前者要用户换设备，后者要用户换参数，
    /// 是两种完全不同的补救动作。
    #[error("无法使用设备 {device}：{reason}")]
    DeviceUnavailable { device: String, reason: String },

    /// 实际达成的采样率低于可用的下限。
    ///
    /// 低于 16 kHz 时 A 计权的 12194 Hz 极点高于 Nyquist，滤波器会被
    /// 钳位甚至不稳定。**直接拒绝运行**比降级运行诚实——降级运行时
    /// 报告出来的 dB(A) 是错的，而用户不会知道。
    #[error("采样率 {actual} Hz 过低（下限 {min} Hz）；蓝牙耳机切到通话模式时会这样")]
    SampleRateTooLow { actual: u32, min: u32 },

    /// 存储路径非法（越界、或不在允许的根目录内）。
    #[error("路径不合法：{0}")]
    InvalidPath(String),

    /// 磁盘操作失败。
    ///
    /// 字段叫 `detail` 而不是 `source`：`thiserror` 会把名为 `source` 的
    /// 字段当成错误链的一环，要求它是一个 `std::error::Error`。这里刻意
    /// 存字符串——`noise-core` 的错误要能跨线程、能序列化，
    /// 而 `std::io::Error` 两样都做不到。
    #[error("{operation} 失败：{detail}")]
    Io {
        operation: &'static str,
        detail: String,
    },
}

impl CoreError {
    /// 这个错误是否值得重试。
    ///
    /// 给引擎的退避重启逻辑用：设备被拔掉值得无限重试（用户会插回来），
    /// 配置不合法重试一万次也还是同一个结果。
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::DeviceUnavailable { .. } | Self::DeviceNotFound(_)
        )
    }
}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io {
            operation: "I/O",
            detail: e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_problems_are_transient() {
        assert!(CoreError::DeviceNotFound("USB 麦克风".into()).is_transient());
        assert!(CoreError::DeviceUnavailable {
            device: "USB 麦克风".into(),
            reason: "被独占".into()
        }
        .is_transient());
    }

    /// 配置错误重试是没有意义的——同一个配置会得到同一个结果。
    #[test]
    fn config_problems_are_not_transient() {
        assert!(!CoreError::InvalidConfig("x".into()).is_transient());
        assert!(!CoreError::SampleRateTooLow {
            actual: 16_000,
            min: 16_000
        }
        .is_transient());
    }

    /// 消息会原样出现在 UI 上，所以要包含具体值而不只是类型名。
    #[test]
    fn messages_carry_the_offending_value() {
        let e = CoreError::SampleRateTooLow {
            actual: 8_000,
            min: 16_000,
        };
        let s = e.to_string();
        assert!(s.contains("8000"), "消息里没有实际采样率：{s}");
        assert!(s.contains("16000"), "消息里没有下限：{s}");
    }
}
