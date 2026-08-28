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
pub mod file_server;
pub mod file_share;
pub mod obs;
pub mod server_conn;
pub mod sync;
pub mod tauri_cmd;
pub mod transfer;
pub mod update;

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Listener, Manager, WindowEvent,
};

use crate::cache::file_cache::FileCache;
use crate::config::AppConfig;
use crate::device::identity::DeviceIdentity;
use crate::device::registry::DeviceRegistry;
use crate::discovery::manual::ManualAddressBook;
use crate::discovery::{DiscoveredPeer, MdnsDiscovery};
use crate::file_share::FileShare;
use crate::server_conn::{CrossLanOffer, ServerConn};
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
    /// 跨局域网文件共享注册表（hash → 本地路径，供内嵌 HTTP 服务拉取）
    pub file_share: Arc<FileShare>,
    /// 跨局域网服务端连接（含已启用节点表 + 启用态）；setup 时建立
    pub server_conn: Mutex<Option<Arc<ServerConn>>>,
    /// 跨 LAN 待复制清单（前端初始化快照用，实时更新走 `cross-lan-file` 事件）
    pub cross_lan_offers: Mutex<Vec<CrossLanOffer>>,
    /// 跨 LAN 文件传输密钥（服务端 Welcome 派生，与文字中继共用 network_key）。
    /// 供内嵌 HTTP 文件服务加密、拉取端解密；未连服务端时为 None。
    pub network_key: Arc<std::sync::Mutex<Option<[u8; 32]>>>,
}

impl AppState {
    pub fn new() -> Self {
        let config = AppConfig::default();
        let identity = DeviceIdentity::load_or_create(&config.device_name)
            .expect("failed to load or create device identity");
        let engine = Arc::new(SyncEngine::new(identity.clone()));
        let hub = ConnectionHub::new(Arc::new(identity.clone()), engine.clone());

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
                file_share: Arc::new(FileShare::new()),
                server_conn: Mutex::new(None),
                cross_lan_offers: Mutex::new(Vec::new()),
                network_key: Arc::new(std::sync::Mutex::new(None)),
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
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // 第二个实例启动时不再新建窗口，而是聚焦已运行的第一个实例主窗口
            show_main_window(app);
        }))
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
                let mut persisted = crate::config::load_config(&handle);
                // 配对码默认随机生成：若为空或仍是出厂默认值 "000000"，则生成一个新的
                // 随机码并持久化，保证每台设备安装后都有独立配对码（无需用户手动设置）。
                if persisted.pairing_code.trim().is_empty()
                    || persisted.pairing_code.trim() == "000000"
                {
                    let new_code = crate::crypto::pake::generate_pairing_code();
                    persisted.pairing_code = new_code;
                    let _ = crate::config::save_config(&handle, &persisted);
                }
                *app.state::<AppState>().config.lock() = persisted;
            }

            // 同步「配对码」到连接中枢：作为 SPAKE2 应答方的常驻口令，
            // 对端首配对时输入本机显示的这个码即可（两端无需预先设成相同）。
            {
                let code = app.state::<AppState>().config.lock().pairing_code.clone();
                app.state::<AppState>().hub.set_pairing_code(code);
                let max_folder_files = app.state::<AppState>().config.lock().max_folder_files;
                app.state::<AppState>().hub.set_max_folder_files(max_folder_files);
            }
            let state = app.state::<AppState>();
            let (enable_mdns, listen_port) = {
                let g = state.config.lock();
                (g.enable_mdns, g.listen_port)
            };
            let identity = state.identity.clone();

            // 恢复已配对设备：设备表从磁盘读，重连口令从密钥链读。必须在传输中枢
            // 启动**之前**装载完，否则监控任务首轮巡检时还认不出这些设备是已配对的。
            {
                let devices = crate::device::store::load_devices(&handle);
                let mut secrets = HashMap::new();
                {
                    let mut reg = state.registry.lock();
                    for d in devices {
                        let id = d.device_id.0.clone();
                        match crate::device::store::load_secret(&handle, &id) {
                            Some(secret) => {
                                secrets.insert(id, secret);
                                reg.add(d);
                            }
                            // 没有口令就无法静默重连。留在表里只会显示成一台永远
                            // 连不上的「已配对」设备，不如剔除，让用户重新配对。
                            None => tracing::warn!(
                                "设备 {} 的配对口令已丢失，需重新配对",
                                d.device_name
                            ),
                        }
                    }
                }
                state.hub.restore_paired(secrets);
            }

            // 启动局域网发现（mDNS 广播本机 + 订阅对端），失败仅记录不阻断启动。
            // 端口来自配置（默认 20071，可改）；发现方从对端广告动态读取端口，不写死。
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

            // 启动跨局域网服务端连接（常连 + 心跳 + 中继路由）；失败仅记录不阻断启动
            {
                let sc = ServerConn::new(
                    app.handle().clone(),
                    app.state::<AppState>().engine.clone(),
                );
                sc.start();
                *app.state::<AppState>().server_conn.lock() = Some(sc);
            }

            // 跨 LAN 文件直取复用上面的 listen_port（由 transfer/manager.rs 的 accept
            // 循环在收到 GET /file/ 时分流），无需在此另起服务。

            // 把跨 LAN 待复制通知同时缓冲进状态，供前端初始化快照
            {
                let app_handle = app.handle().clone();
                let _ = app_handle.clone().listen("cross-lan-file", move |event| {
                    if let Ok(o) =
                        serde_json::from_str::<CrossLanOffer>(event.payload())
                    {
                        app_handle
                            .state::<AppState>()
                            .cross_lan_offers
                            .lock()
                            .push(o);
                    }
                });
            }

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
            tauri_cmd::pair_with,
            tauri_cmd::pair_manual,
            tauri_cmd::regenerate_pairing_code,
            tauri_cmd::unpair,
            tauri_cmd::pull_files,
            tauri_cmd::list_pending_offers,
            tauri_cmd::open_settings,
            tauri_cmd::quit_app,
            hide_app_window,
            win_minimize,
            win_toggle_maximize,
            tauri_cmd::get_server_status,
            tauri_cmd::get_server_nodes,
            tauri_cmd::list_cross_lan_offers,
            tauri_cmd::pull_cross_lan,
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
        // 通知前端窗口已显示：TitleBar 监听此事件强制回流，清除隐藏期间残留的 :hover 红底
        let _ = app.emit("main-shown", ());
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

/// 自定义标题栏「关闭」按钮调用：隐藏主窗口而非退出进程（与点击原生 X 行为一致）
#[tauri::command]
fn hide_app_window(app: tauri::AppHandle) {
    hide_main_window(&app);
}

/// 标题栏「最小化」按钮：走 Rust 命令而非 JS `getCurrentWindow().minimize()`，
/// 因为该 JS 窗口写操作在本项目未被授予权限（日志曾报 allow-minimize not allowed），
/// 而 Rust 侧调用无需前端窗口权限。
#[tauri::command]
fn win_minimize(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.minimize();
    }
}

/// 标题栏「最大化/还原」按钮，理由同上（allow-toggle-maximize not allowed）。
/// WebviewWindow 无 toggle_maximize，手动根据当前状态切换。
#[tauri::command]
fn win_toggle_maximize(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_maximized().unwrap_or(false) {
            let _ = w.unmaximize();
        } else {
            let _ = w.maximize();
        }
    }
}
