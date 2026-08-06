//! ClipSync - 跨平台剪贴板同步工具
//!
//! 模块结构见 docs/development-plan.md 第十章

pub mod cache;
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
    Manager, WindowEvent,
};

use crate::cache::file_cache::FileCache;
use crate::config::AppConfig;
use crate::device::identity::DeviceIdentity;
use crate::device::registry::DeviceRegistry;
use crate::discovery::manual::ManualAddressBook;
use crate::discovery::{DiscoveredPeer, MdnsDiscovery};
use crate::sync::engine::SyncEngine;
use crate::transfer::manager::ConnectionHub;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// 全局应用状态（在 `setup` 之前通过 `manage` 注入）
pub struct AppState {
    pub identity: DeviceIdentity,
    /// 运行期可改的配置（端口等），背后加锁以支持 `set_config` 热更新
    pub config: Mutex<AppConfig>,
    pub engine: Arc<SyncEngine>,
    /// 传输连接中枢：监听 / 连接对端 / SPAKE2 配对 / 加密通道转发
    pub hub: Arc<ConnectionHub>,
    pub registry: Mutex<DeviceRegistry>,
    pub manual: Mutex<ManualAddressBook>,
    pub cache: FileCache,
    /// 局域网发现控制器（mDNS）；feature 关闭时为无操作占位
    pub discovery: MdnsDiscovery,
    /// 当前通过 mDNS 发现的局域网对端（供 `list_discovered_peers` 查询）
    pub discovered: Mutex<HashMap<String, DiscoveredPeer>>,
}

impl AppState {
    pub fn new() -> Self {
        let config = AppConfig::default();
        let identity = DeviceIdentity::load_or_create(&config.device_name)
            .expect("failed to load or create device identity");
        let engine = Arc::new(SyncEngine::new(identity.clone()));
        let hub = ConnectionHub::new(
            Arc::new(identity.clone()),
            engine.clone(),
            config.pairing_code.clone(),
        );

        let mut manual = ManualAddressBook::new();
        for addr in &config.manual_addresses {
            manual.add(addr.clone());
        }

        let cache = FileCache::new(256, config.cache_ttl_hours);

        Self {
                identity,
                config: Mutex::new(config),
                engine,
                hub,
                registry: Mutex::new(DeviceRegistry::new()),
                manual: Mutex::new(manual),
                cache,
                discovery: MdnsDiscovery::new(),
                discovered: Mutex::new(HashMap::new()),
            }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// 应用入口
pub fn run() {
    crate::obs::logging::init_file_logging();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AppState::new())
        .setup(|app| {
            // 启动时显示主窗口（如果未隐藏）
            if let Some(window) = app.get_webview_window("main") {
                if std::env::args().nth(1).as_deref() != Some("--hidden") {
                    window.show()?;
                }
            }

            build_tray(app)?;

            // 加载持久化配置（覆盖默认），使改过的端口等设置重启后仍生效
            let handle = app.handle().clone();
            {
                let persisted = crate::config::load_config(&handle);
                *app.state::<AppState>().config.lock() = persisted;
            }
            let state = app.state::<AppState>();
            let (enable_mdns, listen_port) = {
                let g = state.config.lock();
                (g.enable_mdns, g.listen_port)
            };
            // 用持久化配置里的配对码覆盖 hub 默认，使改过的配对码重启后仍生效
            state
                .hub
                .set_pairing_code(state.config.lock().pairing_code.clone());
            let identity = state.identity.clone();

            // 启动局域网发现（mDNS 广播本机 + 订阅对端），失败仅记录不阻断启动。
            // 端口来自配置（默认 24681，可改）；发现方从对端广告动态读取端口，不写死。
            if enable_mdns {
                if let Err(e) =
                    app.state::<AppState>()
                        .discovery
                        .start(&handle, &identity, listen_port)
                {
                    tracing::error!("mDNS discovery failed to start: {e}");
                }
            }

            // 启动同步引擎（剪贴板监听 + 事件广播），失败仅记录不阻断启动
            let engine = app.state::<AppState>().engine.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = engine.start(handle).await {
                    tracing::error!("sync engine failed to start: {e}");
                }
            });

            // 启动传输中枢（监听 / 连接对端 / SPAKE2 配对 / 加密转发），失败仅记录不阻断启动
            let hub = app.state::<AppState>().hub.clone();
            let hub_app = app.handle().clone();
            let hub_port = listen_port;
            tauri::async_runtime::spawn(async move {
                hub.start(hub_app, hub_port).await;
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            // 仅对主窗口：点关闭按钮（X）隐藏而非退出进程；设置等其它窗口正常关闭
            if window.label() == "main" {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    hide_main_window(window.app_handle());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            tauri_cmd::get_version,
            tauri_cmd::get_device_id,
            tauri_cmd::get_device_name,
            tauri_cmd::get_config,
            tauri_cmd::get_clipboard,
            tauri_cmd::set_clipboard,
            tauri_cmd::get_paired_devices,
            tauri_cmd::add_manual_address,
            tauri_cmd::list_manual_addresses,
            tauri_cmd::remove_manual_address,
            tauri_cmd::cache_stats,
            tauri_cmd::set_config,
            tauri_cmd::list_discovered_peers,
            tauri_cmd::list_connected_peers,
            tauri_cmd::open_settings,
            tauri_cmd::quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// 构建系统托盘图标与菜单
fn build_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let show_i = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let hide_i = MenuItem::with_id(app, "hide", "隐藏主窗口", true, None::<&str>)?;
    let settings_i = MenuItem::with_id(app, "settings", "设置", true, None::<&str>)?;
    let sep_i = PredefinedMenuItem::separator(app)?;
    let quit_i = MenuItem::with_id(app, "quit", "退出 ClipSync", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_i, &hide_i, &settings_i, &sep_i, &quit_i])?;

    let tray_icon = app.default_window_icon().cloned().unwrap_or_else(|| {
        tauri::image::Image::from_path("icons/tray-icon.png").expect("tray-icon.png must exist")
    });

    TrayIconBuilder::with_id("main")
        .icon(tray_icon)
        .icon_as_template(true)
        .tooltip("ClipSync")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "hide" => hide_main_window(app),
            "settings" => {
                if let Err(e) = crate::tauri_cmd::open_settings(app.clone()) {
                    tracing::error!("failed to open settings window: {e}");
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
                        hide_main_window(app);
                    } else {
                        show_main_window(app);
                    }
                }
            }
        })
        .build(app)?;

    // 启动时隐藏 Dock（仅在菜单栏运行）
    #[cfg(target_os = "macos")]
    {
        use tauri::ActivationPolicy;
        let _ = app.set_activation_policy(ActivationPolicy::Accessory);
    }

    Ok(())
}

/// 显示主窗口并恢复 Dock 图标
fn show_main_window(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    {
        use tauri::ActivationPolicy;
        let _ = app.set_activation_policy(ActivationPolicy::Regular);
    }
    if let Some(w) = app.get_webview_window("main") {
        w.show().ok();
        w.set_focus().ok();
    }
}

/// 隐藏主窗口并移除 Dock 图标（仅菜单栏）
fn hide_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        w.hide().ok();
    }
    #[cfg(target_os = "macos")]
    {
        use tauri::ActivationPolicy;
        let _ = app.set_activation_policy(ActivationPolicy::Accessory);
    }
}
