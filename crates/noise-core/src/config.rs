//! 应用级配置。
//!
//! # 为什么整份配置都在 `noise-core`
//!
//! 检测器（`noise-detect`）只认 [`noise_detect::DetectorConfig`]，存储层只认
//! 保留策略的部分，UI 要全部。**让每一层各自定义「它关心的那几项」**看起来
//! 更解耦，实际会立刻退化成三份会分叉的默认值——而配置文件是用户唯一
//! 会手改的东西，分叉意味着「界面显示 15，跑起来是 12」。
//!
//! 这里持有的是**唯一的真相**，其他层通过 [`MonitorConfig`] 的字段访问。
//!
//! # `#[serde(default)]` 是刻意的
//!
//! 每层都标了。新增字段时旧配置文件仍能载入，缺失字段取默认值，而不是
//! 让整个配置反序列化失败、用户看到一个空配置、默认值悄悄生效。**配置
//! 载入失败必须是响亮的**，但「少了一个新字段」不是失败。
//!
//! [`noise_detect::DetectorConfig`]: noise_detect::DetectorConfig

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// 应用的全部可调参数。
// 不 `derive(Default)`：下面有手写的实现，因为 `PlaybackConfig` 等
// 嵌套类型各有自己的非零默认值，逐字段转发比 derive 更清楚。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    pub detector: crate::DetectorConfig,
    pub playback: PlaybackConfig,
    pub recording: RecordingConfig,
    pub calibration: CalibrationConfig,
    pub retention: RetentionPolicy,

    /// 输入设备名。`None` = 系统默认。
    ///
    /// 存**名字**而不是索引：设备的枚举顺序在插拔 USB 麦克风之后会变，
    /// 存索引意味着某天早上开始监测的是另一支麦克风，而用户不会收到
    /// 任何提示。
    pub input_device: Option<String>,

    /// 输出设备名。`None` = 系统默认。
    pub output_device: Option<String>,

    /// 存储根目录。`None` = 平台的默认位置。
    pub store_root: Option<String>,

    /// 是否把 20 Hz 的电平快照推给前端。
    ///
    /// 默认开，但前端在窗口不可见时会关掉它。**App 整夜挂后台还在烧 IPC**
    /// 是这类软件最常见的可耻浪费，而这个开关是唯一能真正掐断它的地方。
    pub level_stream_enabled: bool,
}

/// 反击回放。
///
/// **默认关闭。** 这个功能会升级冲突、可能违反租约或噪音条例，而且自己
/// 回放的日志本身就是证据（见 `docs/manual-test.md` 与计划中的风险 4）。
/// 默认关闭是设计的一部分，不是保守。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackConfig {
    /// 总开关。关闭时**门控根本不构造**（L0），检测路径上不存在门控代码。
    pub enabled: bool,

    /// 回放增益，dB。**运行时可调，默认值故意保守。**
    ///
    /// 计划里用户明确要求「反击声量我后面实测调一下」——所以这里给一个
    /// 明显偏小的起点，而不是一个听起来合理的值。宁可用户第一次测的时候
    /// 觉得「怎么这么轻」，也不要第一次就调到能吵到自己的程度。
    ///
    /// 上界由 [`Self::max_gain_db`] 管：UI 的旋钮能拧多远，不该由用户
    /// 事后发现自己把音响烧了来决定。
    pub gain_db: f64,

    /// 增益的硬上界。
    pub max_gain_db: f64,

    /// 是否在回放前弹确认框。
    ///
    /// 前若干次回放都要确认。走的是同一条回放路径（`Manual` 与自动回放
    /// 不共享任何后门），这个开关只影响 UI 是否拦一道。
    pub confirm_first_n: u32,
}

/// 录音与取证。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingConfig {
    pub enabled: bool,

    /// pre-roll 长度，毫秒。触发**之前**的音频也要留下。
    ///
    /// 15 s 是起点而非答案：如果邻居的模式是「先拖一下椅子、十秒后才
    /// 走路」，需要更长的前奏才能把因果讲清楚。计划里把「15 s 够不够」
    /// 列为需要用数据回答的开放问题。
    pub pre_roll_ms: f64,

    /// 事件结束后的继续录制长度，毫秒。
    ///
    /// 3000 而不是 0：邻居停下来的那一下（椅子落回地面、门关上）常常
    /// 才是最有说服力的一段，而它在「事件已结束」之后。
    pub post_roll_ms: f64,

    /// 单个 WAV 分段的最长长度，毫秒。超长事件轮转成多个文件。
    ///
    /// 30 s：一个 30 s 的 f32 单声道 48 kHz 段是 5.8 MB，崩了也只丢这么多。
    /// 而且每个段独立完整可播——取证材料里「有一个打不开的文件」比
    /// 「少了一段」更难解释。
    pub segment_ms: f64,

    /// 存 f32 还是 24 bit PCM。
    ///
    /// f32 是默认：它无损保留检测器看到的东西，而录音的用途正是事后
    /// 逐样本核对检测器的判定。24 bit 只在磁盘紧张时才有意义。
    pub float_samples: bool,
}

/// 校准。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CalibrationConfig {
    /// 94 dB SPL @ 1 kHz 对应的 dBFS 读数。
    ///
    /// `None` = **未校准**。此时 UI 必须只显示 dBFS，绝不能声称绝对声压级。
    /// 没有声校准器，「邻居达到 68 dB(A)」不是可辩护的陈述——这是技术
    /// 风险，也是预期管理风险（计划中的风险 2）。
    pub dbfs_at_94db_spl: Option<f64>,

    /// 实测的声学耦合，dB。回放时麦克风收到的电平相对回放电平的差值。
    ///
    /// 由 `run_coupling_calibration` 写入，用于抬高有效起始阈值（L3）。
    /// 这是 App 对自己音响「到底有多响」的诚实报告——对「反击能不能
    /// 被邻居听到」是个有用的现实检查。
    pub coupling_db: Option<f64>,

    /// 每个设备的独立校准偏移，按设备名索引。
    ///
    /// 换麦克风就得重校。把偏移绑在设备名上，是为了避免「换了麦之后
    /// 所有历史读数都不再可比，但界面看起来没变」。
    pub per_device_offset_db: std::collections::BTreeMap<String, f64>,
}

/// 保留策略。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionPolicy {
    pub max_age_days: u32,
    pub max_total_bytes: u64,

    /// 永不删除已标记或已注释的事件。
    ///
    /// **这一条不是可调项。** 用户标了「这条要给物业看」的事件被自动清理
    /// 掉，会让整个取证功能失去意义。保留策略只清理无人问津的记录。
    pub keep_flagged: bool,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            // 故意偏小。用户会实测调，起点低是安全的失败方向。
            gain_db: -18.0,
            max_gain_db: 0.0,
            confirm_first_n: 3,
        }
    }
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            pre_roll_ms: 15_000.0,
            post_roll_ms: 3_000.0,
            segment_ms: 30_000.0,
            float_samples: true,
        }
    }
}

impl Default for CalibrationConfig {
    fn default() -> Self {
        Self {
            dbfs_at_94db_spl: None,
            coupling_db: None,
            per_device_offset_db: Default::default(),
        }
    }
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_age_days: 90,
            max_total_bytes: 20 * 1024 * 1024 * 1024,
            keep_flagged: true,
        }
    }
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            detector: Default::default(),
            playback: Default::default(),
            recording: Default::default(),
            calibration: Default::default(),
            retention: Default::default(),
            input_device: None,
            output_device: None,
            store_root: None,
            level_stream_enabled: true,
        }
    }
}

impl MonitorConfig {
    /// 配置是否自洽。
    ///
    /// 在**载入时**调用一次，也在每次 `update_config` 时调用。不合法就
    /// 拒绝，而不是带着一个荒谬的值继续跑——一个 `pre_roll_ms = -1`
    /// 在环形缓冲里会变成一个巨大的 `usize`，然后是一夜之间被吃光的
    /// 内存。
    pub fn validate(&self) -> Result<(), CoreError> {
        self.detector
            .validate()
            .map_err(|e| CoreError::InvalidConfig(e.to_string()))?;

        check_range("playback.gain_db", self.playback.gain_db, -60.0, self.playback.max_gain_db)?;
        check_range("playback.max_gain_db", self.playback.max_gain_db, -60.0, 24.0)?;
        check_range("recording.pre_roll_ms", self.recording.pre_roll_ms, 0.0, 300_000.0)?;
        check_range("recording.post_roll_ms", self.recording.post_roll_ms, 0.0, 300_000.0)?;
        check_range(
            "recording.segment_ms",
            self.recording.segment_ms,
            1_000.0,
            3_600_000.0,
        )?;
        check_range("retention.max_age_days", self.retention.max_age_days as f64, 1.0, 3650.0)?;

        if let Some(c) = self.calibration.dbfs_at_94db_spl {
            // 94 dB SPL 不可能是 +20 dBFS——满刻度就是 0 dBFS。这个检查
            // 挡的是符号写反（-28 写成 +28），那会让所有读数差 56 dB。
            check_range("calibration.dbfs_at_94db_spl", c, -120.0, 0.0)?;
        }
        if let Some(c) = self.calibration.coupling_db {
            check_range("calibration.coupling_db", c, -60.0, 60.0)?;
        }

        if self.playback.enabled && self.output_device.is_none() && self.input_device.is_none() {
            // 不是错误。回放开启但两个设备都用系统默认，是完全正常的配置。
            // 这里刻意什么都不做，只是把这条判断记下来：将来如果想加
            // 「输入输出是同一个设备就警告」的检查，位置在这里。
        }

        Ok(())
    }

    /// pre-roll 长度换算成样本数。**按实际采样率算**，不是写死 48 kHz。
    pub fn pre_roll_samples(&self, fs: f64) -> usize {
        (self.recording.pre_roll_ms / 1000.0 * fs).round().max(0.0) as usize
    }

    /// 后录长度换算成样本数。
    pub fn post_roll_samples(&self, fs: f64) -> usize {
        (self.recording.post_roll_ms / 1000.0 * fs).round().max(0.0) as usize
    }

    /// 分段长度换算成样本数。
    pub fn segment_samples(&self, fs: f64) -> usize {
        (self.recording.segment_ms / 1000.0 * fs).round().max(1.0) as usize
    }

    /// 供 UI 显示的增益上界是否已经超出。
    pub fn gain_is_clamped(&self) -> bool {
        self.playback.gain_db > self.playback.max_gain_db
    }
}

fn check_range(name: &str, value: f64, lo: f64, hi: f64) -> Result<(), CoreError> {
    if !value.is_finite() {
        return Err(CoreError::InvalidConfig(format!("{name} 不是有限数：{value}")));
    }
    if value < lo || value > hi {
        return Err(CoreError::InvalidConfig(format!(
            "{name} = {value}，超出允许范围 [{lo}, {hi}]"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        MonitorConfig::default().validate().unwrap();
    }

    /// 回放**默认关闭**是设计的一部分，不是遗漏。这条测试把它钉住：
    /// 哪天有人「顺手」改成 true，这条会红。
    #[test]
    fn playback_is_off_by_default() {
        assert!(!MonitorConfig::default().playback.enabled);
    }

    /// 未校准是默认状态，UI 因此必须只显示 dBFS。
    #[test]
    fn calibration_starts_absent() {
        assert!(MonitorConfig::default().calibration.dbfs_at_94db_spl.is_none());
    }

    #[test]
    fn roll_lengths_scale_with_sample_rate() {
        let c = MonitorConfig::default();
        assert_eq!(c.pre_roll_samples(48_000.0), 720_000);
        assert_eq!(c.pre_roll_samples(44_100.0), 661_500);
        assert_eq!(c.post_roll_samples(48_000.0), 144_000);
        assert_eq!(c.segment_samples(48_000.0), 1_440_000);
    }

    /// 符号写反是这里唯一现实会犯的错，而它会让所有读数差 56 dB。
    #[test]
    fn positive_calibration_value_is_rejected() {
        let mut c = MonitorConfig::default();
        c.calibration.dbfs_at_94db_spl = Some(28.0);
        assert!(c.validate().is_err());
        c.calibration.dbfs_at_94db_spl = Some(-28.0);
        c.validate().unwrap();
    }

    #[test]
    fn negative_pre_roll_is_rejected() {
        let mut c = MonitorConfig::default();
        c.recording.pre_roll_ms = -1.0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn gain_above_the_ceiling_is_rejected() {
        let mut c = MonitorConfig::default();
        c.playback.gain_db = 12.0;
        assert!(c.validate().is_err());
    }

    /// 旧配置文件缺字段时必须能载入，否则每次加旋钮都会让老用户
    /// 看到一个空配置。
    #[test]
    fn partial_json_loads_with_defaults() {
        let c: MonitorConfig = serde_json::from_str(r#"{"playback":{"enabled":true}}"#).unwrap();
        assert!(c.playback.enabled);
        assert_eq!(c.recording.pre_roll_ms, 15_000.0);
        assert_eq!(c.detector.min_rise_db, 15.0);
        c.validate().unwrap();
    }
}
