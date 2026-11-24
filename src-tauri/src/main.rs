// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 二进制的全部内容就是调用 lib。保持 `main.rs` 极薄，让逻辑住在 lib 里，
//! 这样它才能被集成测试引用。

fn main() {
    anti_noise_lib::run()
}
