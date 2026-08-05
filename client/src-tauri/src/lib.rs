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

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};
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

            build_tray(app)?;

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

/// 构建系统托盘图标与菜单
fn build_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let show_i = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let hide_i = MenuItem::with_id(app, "hide", "隐藏主窗口", true, None::<&str>)?;
    let sep_i = PredefinedMenuItem::separator(app)?;
    let quit_i = MenuItem::with_id(app, "quit", "退出 ClipSync", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_i, &hide_i, &sep_i, &quit_i])?;

    let tray_icon = app
        .default_window_icon()
        .cloned()
        .unwrap_or_else(|| {
            tauri::image::Image::from_path("icons/tray-icon.png")
                .expect("tray-icon.png must exist")
        });

    TrayIconBuilder::with_id("main")
        .icon(tray_icon)
        .icon_as_template(true)
        .tooltip("ClipSync")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(w) = app.get_webview_window("main") {
                    w.show().ok();
                    w.set_focus().ok();
                }
            }
            "hide" => {
                if let Some(w) = app.get_webview_window("main") {
                    w.hide().ok();
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(w) = app.get_webview_window("main") {
                    if w.is_visible().unwrap_or(false) {
                        w.hide().ok();
                    } else {
                        w.show().ok();
                        w.set_focus().ok();
                    }
                }
            }
        })
        .build(app)?;

    Ok(())
}

fn init_logging() {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,clipsync=debug"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .init();
}
