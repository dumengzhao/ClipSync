//! ClipSync - 跨平台剪贴板同步工具
//!
//! 模块结构见 docs/development-plan.md 第十章

pub mod clipboard;
pub mod config;
pub mod crypto;
pub mod device;
pub mod discovery;
pub mod error;
pub mod obs;
pub mod sync;
pub mod tauri_cmd;
pub mod transfer;
pub mod update;

use tauri::Manager;
use tracing_subscriber::EnvFilter;

/// 应用入口
pub fn run() {
    init_logging();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            // 启动时显示主窗口（如果未隐藏）
            if let Some(window) = app.get_webview_window("main") {
                if std::env::args().nth(1).as_deref() != Some("--hidden") {
                    window.show()?;
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            tauri_cmd::get_version,
            tauri_cmd::get_device_id,
            tauri_cmd::get_paired_devices,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn init_logging() {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,clipsync=debug"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .init();
}
