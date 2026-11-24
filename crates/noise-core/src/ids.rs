//! 会话与事件标识符。
//!
//! # 为什么 ID 里带时间戳
//!
//! 这些 ID 会出现在**证据材料**里：目录名、导出报告、跟物业的往来。
//! 一个纯随机 ID 或自增序号在那里毫无意义，而
//! `20260912T210512Z-0001` 一眼就能看出「9 月 12 日晚 9 点 05 分，那次
//! 的第一次」。目录列表按名字排序就等于按时间排序。
//!
//! # 为什么是 UTC 而不是本地时间
//!
//! 本地时间会随夏令时来回跳，而且用户改时区之后历史记录的时间戳含义
//! 就变了。**证据的时间戳不能有歧义**，所以存 UTC，显示时才转本地。
//!
//! # 短随机后缀不是装饰
//!
//! 同一秒内重启两次监测会产生两个同名会话，后一个会把前一个的目录
//! 覆盖掉。后缀把碰撞概率压到可以忽略——不是靠祈祷，靠四位 base36。

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// 一次监测会话的标识。形如 `20260912T210433Z-a1b2`。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionId(String);

/// 一次噪声事件的标识。形如 `evt_20260912T210512Z-0001`。
///
/// 字典序 == 时间序（同一次会话内），所以 `events.jsonl` 不需要为了
/// 排序而载入内存。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventId(String);

/// 一次反击回放的标识。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ResponseId(String);

impl SessionId {
    /// 用给定的 UTC 时间构造。
    ///
    /// `unix_ms` 是 Unix 纪元以来的毫秒数，`suffix` 是调用方提供的
    /// 短随机串。**时间由调用方传入而不是在这里读时钟**：这样整个
    /// 序列化的行为在测试里是确定的，不需要 mock 时钟。
    pub fn new(unix_ms: i64, suffix: &str) -> Self {
        Self(format!("{}-{}", compact_utc(unix_ms), sanitize(suffix, 4)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl EventId {
    /// 用会话 ID 和一个会话内单调递增的序号构造。
    pub fn new(session: &SessionId, seq: u64) -> Self {
        Self(format!("evt_{}-{seq:04}", session.as_str()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ResponseId {
    pub fn new(event: &EventId, seq: u64) -> Self {
        Self(format!("rsp_{}-{seq:02}", event.as_str()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for ResponseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Unix 毫秒 → `20260912T210433Z`。
///
/// 手工做历法换算而不是引 `chrono`：这里只需要一个方向、只需要 UTC、
/// 而且必须有确定的测试向量。引一个日期库来做一个除法，然后让它的
/// 版本更新决定文件名的格式，是不划算的。
///
/// 算法是 Howard Hinnant 的 `civil_from_days`（days-from-civil 的逆），
/// 对 1970 年之前和闰年都成立。
pub fn compact_utc(unix_ms: i64) -> String {
    let secs = unix_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);

    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// 天数（相对 1970-01-01）→ (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    // 把纪元挪到 0000-03-01，让闰日落在年末，从而消掉闰年特判。
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]，3 月为 0
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 把任意字符串压成 `max` 个 ASCII 字母或数字。
///
/// 设备名和用户输入会流进文件名，所以这一步同时是**安全边界**：
/// 路径分隔符、`..`、Windows 保留字符全部在这里被消掉。
fn sanitize(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(max);
    for c in s.chars() {
        if out.len() >= max {
            break;
        }
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        }
    }
    if out.is_empty() {
        // 全被消掉了（比如输入是纯符号）。给一个确定的兜底，
        // 而不是返回空串——空串会让 `SessionId` 变成 `...-`，
        // 两个这样的会话看起来一模一样。
        out.push_str("0000");
    }
    out
}

/// 会话内事件序号的来源。
///
/// 单独做成类型而不是让调用方自己 `+= 1`：序号必须**单调且不重复**，
/// 而「我在哪个分支里忘了加」是这类 bug 的典型形态。这里用原子的
/// `fetch_add`，把保证放进类型里。
#[derive(Debug)]
pub struct SeqCounter(AtomicU64);

impl SeqCounter {
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// 取下一个序号，从 1 开始。
    ///
    /// 从 1 而不是 0：`-0000` 看起来像一个初始化失败的占位值，
    /// 而 `-0001` 明确是「第一次」。
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 当前已经发出去多少个。
    pub fn issued(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for SeqCounter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 历法换算必须对：闰年、月末、纪元本身，以及 1970 年之前
    /// （`div_euclid` 而不是 `%` 在这里是有意义的）。
    #[test]
    fn utc_formatting_matches_known_instants() {
        assert_eq!(compact_utc(0), "19700101T000000Z");
        assert_eq!(compact_utc(1_000), "19700101T000001Z");
        // 2000-02-29 是闰日；前一天和后天都要对
        assert_eq!(compact_utc(951_782_400_000), "20000229T000000Z");
        assert_eq!(compact_utc(951_868_800_000), "20000301T000000Z");
        // 2026-09-12 21:04:33 UTC
        assert_eq!(compact_utc(1_789_247_073_000), "20260912T210433Z");
        // 纪元之前（负数）不能变成垃圾
        assert_eq!(compact_utc(-86_400_000), "19691231T000000Z");
    }

    /// 毫秒部分不能渗进秒里，也不能因为负数而错位。
    #[test]
    fn milliseconds_do_not_leak_into_seconds() {
        assert_eq!(compact_utc(999), "19700101T000000Z");
        assert_eq!(compact_utc(1_999), "19700101T000001Z");
        assert_eq!(compact_utc(-1), "19691231T235959Z");
    }

    #[test]
    fn session_and_event_ids_are_sortable_by_time() {
        let a = SessionId::new(1_789_247_073_000, "a1b2");
        let b = SessionId::new(1_789_247_074_000, "a1b2");
        assert!(a < b, "{a} 应排在 {b} 之前");

        let e1 = EventId::new(&a, 1);
        let e2 = EventId::new(&a, 2);
        let e10 = EventId::new(&a, 10);
        assert!(e1 < e2 && e2 < e10, "序号要按数值排，不是按字符");
        assert_eq!(e1.as_str(), "evt_20260912T210433Z-a1b2-0001");
    }

    /// 序号从 1 开始，且绝不重复。
    #[test]
    fn sequence_starts_at_one_and_never_repeats() {
        let c = SeqCounter::new();
        assert_eq!(c.next(), 1);
        assert_eq!(c.next(), 2);
        assert_eq!(c.issued(), 2);
    }

    /// 用户输入会流进文件名，所以消歧必须挡住路径穿越。
    #[test]
    fn suffix_cannot_escape_the_directory() {
        assert_eq!(sanitize("../../etc", 4), "etc");
        assert_eq!(sanitize("a\\b/c:d", 4), "abcd");
        assert_eq!(sanitize("…", 4), "0000");
        assert_eq!(sanitize("", 4), "0000");
        // 不能超长
        assert_eq!(sanitize("abcdefghij", 4).len(), 4);
    }

    #[test]
    fn response_id_is_derived_from_the_event() {
        let s = SessionId::new(0, "zz");
        let e = EventId::new(&s, 7);
        assert_eq!(ResponseId::new(&e, 1).as_str(), "rsp_evt_19700101T000000Z-zz-0007-01");
    }
}
