//! Anti-Noise 的 Tauri 外壳。
//!
//! Phase 0 只搭骨架：一个 `get_status` 命令，用来证明 IPC 通路是活的。

use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorState {
    Stopped,
    Idle,
    Recording,
    Playback,
    Cooldown,
    Suspended,
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitorStatus {
    pub state: MonitorState,
}

#[tauri::command]
fn get_status() -> MonitorStatus {
    MonitorStatus {
        state: MonitorState::Stopped,
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![get_status])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
