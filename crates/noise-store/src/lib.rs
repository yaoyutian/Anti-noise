//! 磁盘层：WAV 录音分段、事件索引、会话清单、保留策略、取证导出。

#[cfg(test)]
// 这个 crate 目前只有骨架（Phase 4 才填内容），占位测试除「能编译」外
// 无可断言。等 `layout`/`segment`/`index` 落地时这条连同 allow 一起删。
#[allow(clippy::assertions_on_constants)]
mod tests {
    #[test]
    fn store_crate_compiles() {
        assert!(true);
    }
}
