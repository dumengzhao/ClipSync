//! Tauri 命令定义
//!
//! 前端通过 `invoke()` 调用这些命令

use serde::Serialize;
use tauri::{AppHandle, State};

use tauri_plugin_autostart::ManagerExt;

// `pull_cross_lan` 需要 emit 拉取结果给「待拉取小窗」（release 同样需要），
// 故 Emitter 不再限定 debug 构建。
use tauri::Emitter;
// `get_webview_window` 在 `show/hide_pull_toast`（Windows 小窗显隐，release 同样需要）
// 与 `debug_simulate_offer`（仅 debug）中均使用，故 `Manager` 必须在 release 也导入，
// 不能限定 debug——否则 release 构建会报 `get_webview_window` 找不到。
use tauri::Manager;

use crate::clipboard::types::ClipboardContent;
use crate::clipboard::ClipboardProvider;
use crate::config::settings::ManualAddress;
use crate::device::registry::{PairedDevice, TrustLevel};
use crate::discovery::DiscoveredPeer;
use crate::discovery::manual::ManualAddressBook;
use crate::AppState;

#[cfg(debug_assertions)]
use crate::transfer::websocket::FileFrame;
#[cfg(debug_assertions)]
use crate::transfer::manager::Outgoing;
#[cfg(debug_assertions)]
use crate::clipboard::types::FileMeta;
#[cfg(debug_assertions)]
use tokio::sync::mpsc;
#[cfg(debug_assertions)]
use std::time::{SystemTime, UNIX_EPOCH};

#[tauri::command]
pub fn get_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[tauri::command]
pub fn get_device_id(state: State<AppState>) -> String {
    state.identity.id.0.clone()
}

#[tauri::command]
pub fn get_device_name(state: State<AppState>) -> String {
    state.config.lock().device_name.clone()
}

#[tauri::command]
pub fn get_config(state: State<AppState>) -> crate::config::AppConfig {
    state.config.lock().clone()
}

/// 更新并持久化配置（如监听端口）。
///
/// - 校验端口范围（1..=65535）
/// - 写入磁盘，重启后仍生效
/// - 若启用 mDNS 且端口变化，立即按新端口重新广播（端口为可配置默认，不写死）
#[tauri::command]
pub fn set_config(
    state: State<AppState>,
    app: AppHandle,
    cfg: crate::config::AppConfig,
) -> Result<(), String> {
    if cfg.listen_port == 0 {
        return Err("listen_port 不能为 0（有效范围 1..=65535）".to_string());
    }

    // ext_file_ep 仅作「本机对外可达 IP」通告，端口恒为 listen_port、不另起服务：
    // 存库时只保留 host（去掉用户可能误填的 :port），避免拉取端拼出错误端口。
    let mut cfg = cfg;
    cfg.ext_file_ep = cfg
        .ext_file_ep
        .split(':')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();

    crate::config::save_config(&app, &cfg).map_err(|e| e.to_string())?;
    *state.config.lock() = cfg.clone();

    // 开机自启：配置切换立即注册/移除系统自启条目（与启动时对齐逻辑一致）
    {
        let mgr = app.autolaunch();
        if let Err(e) = if cfg.auto_start { mgr.enable() } else { mgr.disable() } {
            tracing::warn!("autostart 切换失败: {e}");
        }
    }

    // 同步「配对码」到连接中枢（应答方首配对的常驻口令）
    state.hub.set_pairing_code(cfg.pairing_code.clone());
    // 同步「文件夹文件数上限」到连接中枢（复制文件夹超限拦截阈值）
    state.hub.set_max_folder_files(cfg.max_folder_files);

    // 同步手动地址簿：配置是持久化真源，每次保存都据此重建内存地址簿，
    // 使监控任务的兜底直连始终读到最新地址（重启时 lib.rs 也已据此初始化）。
    {
        let mut book = state.manual.lock();
        *book = ManualAddressBook::new();
        for a in &cfg.manual_addresses {
            book.add(a.clone());
        }
    }

    if cfg.enable_mdns {
        let identity = state.identity.clone();
        if let Err(e) = state
            .discovery
            .reconfigure(&app, &identity, cfg.listen_port)
        {
            tracing::warn!("mDNS reconfigure on port change failed: {e}");
        }
    }

    // 跨局域网服务端配置可能已变更：唤醒连接循环立即重连（无需重启应用）。
    // 重连会让服务端刷新本机 ext_file_ep（对外可达 IP）等节点信息，其它在线设备实时可见。
    {
        let sc = state.server_conn.lock();
        if let Some(sc) = sc.as_ref() {
            sc.reconnect();
        }
    }

    Ok(())
}

/// 打开（或聚焦已存在的）设置窗口。
///
/// 设置窗口与主窗口**共享同一个前端 bundle**，仅通过窗口 label 区分页面：
/// 前端读取 `getCurrentWindow().label`，为 `"settings"` 时渲染 `SettingsPage`，
/// 否则渲染 `App`。因此这里统一用 `WebviewUrl::App("/")`——Tauri 在 dev/build 下
/// 自动映射到正确的前端地址（dev 下指向 Vite，build 下指向打包资源），**不写死任何
/// 端口**，且加载机制与主窗口完全一致，可避免早期用 `External(devUrl + "#/settings")`
/// 时出现的空白页问题。
/// 退出整个应用（供设置窗口的「退出 ClipSync」按钮调用，等价于托盘「退出 ClipSync」）
#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
pub fn open_settings(app: AppHandle) -> Result<(), String> {
    // 统一用主窗口内嵌显示设置视图：显示主窗口并置顶，再 emit 事件让前端切到 settings。
    // 不再为 release 建独立「settings」窗口——前端以 view 状态渲染（默认 main），
    // 独立窗未 emit open-settings 会停在主视图，表现为「托盘点设置打不开设置页」。
    if let Some(w) = tauri::Manager::get_webview_window(&app, "main") {
        // 窗口若已被最小化到任务栏，Windows 上 show()/set_focus() 不会还原（SW_SHOW 不还原
        // 最小化窗口），表现为「打开过主界面并最小化后再点设置没反应」。必须先 unminimize()
        // 才能正确弹到前台。
        if w.is_minimized().unwrap_or(false) {
            let _ = w.unminimize();
        }
        let _ = w.show();
        let _ = w.set_focus();
    }
    app.emit("open-settings", ()).map_err(|e| e.to_string())
}

/// 列出当前通过 mDNS 发现的局域网对端。
///
/// 已配对设备不会出现在发现列表里——它们归「已配对」列表管理，无需在发现区
/// 重复出现（用户也明确要求「已保存的配对不要出现在发现里」）。注意这里只过滤
/// 返回给前端的拷贝，内存中的 `state.discovered` 表保持完整，重连监控仍会据此
/// 对「已发现 + 已配对」的对端发起重连。
#[tauri::command]
pub fn list_discovered_peers(state: State<AppState>) -> Vec<DiscoveredPeer> {
    let paired: std::collections::HashSet<String> = state
        .registry
        .lock()
        .list()
        .into_iter()
        .map(|p| p.device_id.0)
        .collect();
    state
        .discovered
        .lock()
        .values()
        .filter(|p| !paired.contains(&p.device_id))
        .cloned()
        .collect()
}

/// 列出当前已建立加密通道的对端 device_id。
///
/// 前端挂载时主动拉取一次，兜底 `peer-connected` 事件可能早于监听就绪而丢失的竞态。
#[tauri::command]
pub fn list_connected_peers(state: State<AppState>) -> Vec<String> {
    state.hub.connected_peer_ids()
}

/// 用户在前端手动发起配对：作为发起方，使用用户输入的对方配对码连接指定对端。
///
/// 对端只需在其界面显示同一个配对码即可（首配对时本机作为应答方用它当 SPAKE2 口令），
/// 两端无需预先配置相同值。device_id 取自局域网发现列表。
#[tauri::command]
pub fn pair_with(state: State<AppState>, device_id: String, code: String) -> Result<(), String> {
    let peer = {
        let g = state.discovered.lock();
        g.get(&device_id).cloned()
    };
    let peer = peer.ok_or_else(|| format!("未发现设备 {device_id}（请确认对方已上线且在局域网内）"))?;
    let hub = state.hub.clone();
    tauri::async_runtime::spawn(async move {
        hub.pair_with(peer, code).await;
    });
    Ok(())
}

/// 通过手动地址（跨网络 / mDNS 被拦截）发起首配对：用用户输入的对方配对码
/// 作为 SPAKE2 口令连接对端。两端配对码各自独立、无需预先相同。
#[tauri::command]
pub fn pair_manual(
    state: State<AppState>,
    addr: String,
    port: u16,
    code: String,
) -> Result<(), String> {
    if addr.trim().is_empty() {
        return Err("地址不能为空".to_string());
    }
    let hub = state.hub.clone();
    tauri::async_runtime::spawn(async move {
        hub.pair_with_manual_address(addr, port, code).await;
    });
    Ok(())
}

/// 重新生成本机「配对码」并立即持久化、同步到连接中枢。
///
/// 新码即刻生效（应答方首配对用它当 SPAKE2 口令）；已配对设备不受影响（它们走 link secret
/// 重连，不依赖本机配对码）。返回新生成的配对码供前端展示。
#[tauri::command]
pub fn regenerate_pairing_code(state: State<AppState>, app: AppHandle) -> Result<String, String> {
    let new_code = crate::crypto::pake::generate_pairing_code();
    {
        let mut cfg = state.config.lock();
        cfg.pairing_code = new_code.clone();
        crate::config::save_config(&app, &cfg).map_err(|e| e.to_string())?;
    }
    state.hub.set_pairing_code(new_code.clone());
    Ok(new_code)
}

/// 取消与某设备的配对：删除持久化的重连口令与设备记录，并断开当前连接。
///
/// 对端仍会保留自己那一侧的记录，但因本端不再接受它的静默重连，
/// 它会停在「离线」状态；双方都需重新配对才能恢复同步。
#[tauri::command]
pub fn unpair(state: State<AppState>, device_id: String) {
    state.hub.unpair(&device_id);
}

/// 拉取某次文件传输：从对端下载到本机 `sync_dir`，完成后自动写本机剪贴板。
#[tauri::command]
pub fn pull_files(state: State<AppState>, transfer_id: String) {
    let hub = state.hub.clone();
    tauri::async_runtime::spawn(async move {
        hub.pull_files(transfer_id).await;
    });
}

/// 返回当前接收到的「待拉取」文件清单（前端挂载时主动拉取一次，兜底事件丢失）。
#[tauri::command]
pub fn list_pending_offers(state: State<AppState>) -> Vec<serde_json::Value> {
    state.hub.pending_offers_snapshot()
}

/// 弹出「待拉取文件」小窗，定位统一由 Rust 负责：
/// macOS 落在屏幕右上角（跟随菜单栏托盘），Windows 落在右下角（跟随任务栏）。
/// 前端只在待拉取清空时主动 hide()，避免各端坐标错位。
#[tauri::command]
pub fn show_pull_toast(app: AppHandle) {
    crate::transfer::manager::ConnectionHub::show_pull_toast(&app);
}

/// 收起待拉取小窗。
///
/// macOS 补偿逻辑：小窗 show 时用了 `set_focus()`（可见性必需），这会让整个 App 被激活；
/// 等小窗 hide 之后，系统会自动把该 App 的下一个窗口顶上来 —— 表现为
/// 「关掉小窗却弹出主窗口」。这里在隐藏前记下主窗口原本的可见性，若它原本不可见
/// 却被系统带出来了，就延迟一小段再把它重新隐藏。
#[tauri::command]
pub fn hide_pull_toast(app: AppHandle) {
    // 仅在 macOS 分支使用；Windows/Linux 构建该 cfg 块被排除，故用 `_` 前缀静默
    // 「unused variable」警告（macOS 下仍正常读取，不影响补偿逻辑）。
    let _main_was_visible = app
        .get_webview_window("main")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);

    if let Some(w) = app.get_webview_window("pull-toast") {
        let _ = w.hide();
        // Windows：Tauri 的 hide() 在 RDP/受限会话下同样可能静默失效（与 show() 同一类问题），
        // 这里用 Win32 ShowWindow(SW_HIDE) 兜底，确保「自动关闭」一定生效。
        // 注意：前端日志里的「hide_pull_toast 成功」只代表 invoke 返回成功，不代表窗口真隐藏了。
        #[cfg(windows)]
        if let Ok(h) = w.hwnd() {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{IsWindowVisible, ShowWindow, SW_HIDE};
            let hwnd = HWND(h.0 as *mut std::ffi::c_void);
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
                if IsWindowVisible(hwnd).as_bool() {
                    tracing::warn!("hide_pull_toast(win32): ShowWindow(SW_HIDE) 后窗口仍可见");
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    if !_main_was_visible {
        let app2 = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let app3 = app2.clone();
            let _ = app2.run_on_main_thread(move || {
                if let Some(m) = app3.get_webview_window("main") {
                    if m.is_visible().unwrap_or(false) {
                        let _ = m.hide();
                        tracing::info!("hide_pull_toast: 主窗口被系统带出，已重新隐藏");
                    }
                }
            });
        });
    }
}

/// [DEBUG] 前端挂载上报：确认每个窗口实际渲染了哪个分支。
/// pull-toast 窗口必须渲染 PullToast；若渲染成 App，说明 `main.tsx` 的 `isToast`
/// 判断失效 —— 这类问题只看 Rust 日志永远发现不了，因为窗口照样 `is_visible()==true`，
/// 只是内容不对，用户自然认为"没弹"。
#[cfg(debug_assertions)]
#[tauri::command]
pub fn debug_report_mount(label: String, is_toast: bool) {
    tracing::info!(
        "[MOUNT] 窗口 {label} 挂载, is_toast={is_toast}, 渲染={}",
        if is_toast { "PullToast" } else { "App" }
    );
}

/// [DEBUG] 小窗诊断日志：前端把关键节点上报到 Rust 日志。
/// 用于排查「窗口弹了但内容为空」「点关闭没反应」这类只看截图/日志查不出的前端问题。
#[cfg(debug_assertions)]
#[tauri::command]
pub fn debug_toast_log(msg: String) {
    tracing::info!("[TOAST] {msg}");
}

/// [DEBUG] 模拟一次**跨 LAN** 文件通知：走与真实跨 LAN 完全相同的链路
/// （emit cross-lan-file → show_pull_toast），用于验证小窗对跨 LAN 条目是否弹出。
#[cfg(debug_assertions)]
#[tauri::command]
pub fn simulate_cross_lan_offer(app: AppHandle) {
    // 每次生成不同的 key：真实场景下不同文件/不同时刻的通知 key 也不同，
    // 若固定 key 会被前端去重逻辑挡掉，测不出"第二次弹窗"的真实行为。
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let manifest = serde_json::json!([
        { "file_name": format!("跨LAN演示文件-{now}.zip"), "file_size": 3 * 1024 * 1024, "is_dir": false }
    ]);
    let offer = crate::server_conn::CrossLanOffer {
        from: format!("cross-lan-sim-{now}"),
        from_name: "跨LAN模拟设备".to_string(),
        manifest,
        ext_file_ep: format!("127.0.0.1:{}", 50000 + (now % 1000) as u16),
    };
    let _ = app.emit("cross-lan-file", offer);
    // 与真实跨 LAN 路径保持一致：通知后弹出待拉取小窗
    crate::transfer::manager::ConnectionHub::show_pull_toast(&app);
    tracing::info!("SIM-CROSS-LAN 已发出模拟跨 LAN 文件通知");
}

/// [DEBUG] 手动模拟一次对端文件 Offer：走与真实对端完全相同的接收链路
/// （写 pending_offers → emit file-offer → show_pull_toast），用于在无对端时验证
/// 「待拉取小窗」是否真的弹出、定位是否正确。需在前端 / DevTools 主动调用，不会自动触发。
#[cfg(debug_assertions)]
#[tauri::command]
pub async fn simulate_incoming_offer(app: AppHandle) {
    debug_simulate_offer(app).await;
}

#[cfg(debug_assertions)]
pub(crate) async fn debug_simulate_offer(app: AppHandle) {
    let (tx, _rx) = mpsc::unbounded_channel::<Outgoing>();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let frame = FileFrame::Offer {
        transfer_id: format!("sim-{now}"),
        device_id: "sim-device-0000".to_string(),
        device_name: "模拟设备A".to_string(),
        files: vec![FileMeta {
            file_name: "演示文件.txt".to_string(),
            // 5MB：刻意超过 auto_pull 阈值（默认 1MB），这样不会走自动拉取，
            // 小窗会常驻显示「拉取」按钮，便于人工确认窗口是否弹出、位置是否正确。
            // （若小于阈值会自动拉取 → 因模拟对端不存在而失败，小窗一闪而过不好判断）
            file_size: 5 * 1024 * 1024,
            is_dir: false,
            relative_path: "演示文件.txt".to_string(),
            modified_at: 0,
            mime_type: "text/plain".to_string(),
            hash: None,
        }],
        top_names: vec!["演示文件.txt".to_string()],
        has_folder: false,
    };
    let hub = app.state::<AppState>().hub.clone();
    hub.handle_file_frame(frame, &tx, "sim").await;
    // 延迟检查 pull-toast 真实可见/聚焦状态，做实证
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        if let Some(w) = app2.get_webview_window("pull-toast") {
            let vis = w.is_visible().unwrap_or(false);
            let foc = w.is_focused().unwrap_or(false);
            tracing::info!("SIM-OFFER pull-toast 状态: visible={vis} focused={foc}");
        } else {
            tracing::warn!("SIM-OFFER pull-toast 窗口不存在，无法验证");
        }
    });
}

// 注意：以下两个命令必须复用引擎持有的常驻剪贴板句柄，不能自建临时实例。
// X11/Wayland 下临时实例一旦 Drop 就会失去 selection 所有权，写入等于白写。

#[tauri::command]
pub async fn get_clipboard(state: State<'_, AppState>) -> Result<String, String> {
    let cb = state.engine.clipboard();
    match cb.read().await {
        Ok(ClipboardContent::Text(t)) => Ok(t),
        Ok(_) => Err("clipboard does not contain text".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn set_clipboard(state: State<'_, AppState>, text: String) -> Result<(), String> {
    let cb = state.engine.clipboard();
    cb.write(ClipboardContent::Text(text))
        .await
        .map_err(|e| e.to_string())
}

/// 已配对设备信息（转发给前端）
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub fingerprint: String,
    pub trusted: bool,
    pub last_seen: u64,
    /// 对端最后一次出现的可拨号地址（host:port），供前端展示和兜底重连参考
    pub last_addr: Option<String>,
}

impl From<PairedDevice> for DeviceInfo {
    fn from(p: PairedDevice) -> Self {
        Self {
            id: p.device_id.0,
            name: p.device_name,
            fingerprint: p.fingerprint,
            trusted: matches!(p.trust, TrustLevel::Verified),
            last_seen: p.last_seen,
            last_addr: p.last_addr,
        }
    }
}

#[tauri::command]
pub fn get_paired_devices(state: State<AppState>) -> Vec<DeviceInfo> {
    state
        .registry
        .lock()
        .list()
        .into_iter()
        .map(DeviceInfo::from)
        .collect()
}

#[tauri::command]
pub fn add_manual_address(state: State<AppState>, addr: ManualAddress) -> Result<(), String> {
    state.manual.lock().add(addr);
    Ok(())
}

#[tauri::command]
pub fn list_manual_addresses(state: State<AppState>) -> Vec<ManualAddress> {
    state.manual.lock().list()
}

#[tauri::command]
pub fn remove_manual_address(state: State<AppState>, label: String) -> Result<(), String> {
    if state.manual.lock().remove(&label) {
        Ok(())
    } else {
        Err(format!("manual address '{label}' not found"))
    }
}

/// 缓存统计（调试用）
#[derive(Debug, Clone, Serialize)]
pub struct CacheStats {
    pub entries: usize,
}

#[tauri::command]
pub fn cache_stats(state: State<AppState>) -> CacheStats {
    CacheStats {
        entries: state.cache.len(),
    }
}

/// 跨局域网服务端连接状态（0 未连接 / 1 待审批 / 2 已启用）。
#[tauri::command]
pub fn get_server_status(state: State<AppState>) -> u8 {
    match state.server_conn.lock().as_ref() {
        Some(sc) => match sc.status() {
            crate::server_conn::ServerStatus::Disconnected => 0,
            crate::server_conn::ServerStatus::Pending => 1,
            crate::server_conn::ServerStatus::Active => 2,
        },
        None => 0,
    }
}

/// 跨局域网已启用节点列表（来自服务端下发的 nodes_update）。
#[tauri::command]
pub fn get_server_nodes(state: State<AppState>) -> Vec<crate::server_conn::RemoteNode> {
    state
        .server_conn
        .lock()
        .as_ref()
        .map(|sc| sc.nodes())
        .unwrap_or_default()
}

/// 当前跨 LAN「待复制」清单（前端初始化快照；实时更新走 `cross-lan-file` 事件）。
#[tauri::command]
pub fn list_cross_lan_offers(state: State<AppState>) -> Vec<crate::server_conn::CrossLanOffer> {
    state.cross_lan_offers.lock().clone()
}

/// 拉取某条跨 LAN 文件通知：从对端 ext_file_ep 下载并写本机剪贴板。
/// `pull_id` 是前端待拉取条目 id（不含 `local:` 前缀），用于把进度/完成事件
/// 精确投递到对应条目；失败时补发 `file-pull-complete(ok:false)` 便于前端提示。
#[tauri::command]
pub async fn pull_cross_lan(
    app: AppHandle,
    state: State<'_, AppState>,
    pull_id: String,
    ext_file_ep: String,
    manifest: serde_json::Value,
) -> Result<(), String> {
    let sc = state.server_conn.lock().clone();
    let sc = sc.ok_or_else(|| "服务端未连接".to_string())?;
    let r = sc
        .pull_cross_lan(&pull_id, &ext_file_ep, manifest)
        .await
        .map_err(|e| e.to_string());
    // 拉取过程由 server_conn 实时上报 file-pull-progress / file-pull-complete(ok:true)。
    // 仅当整条拉取失败（如网络不可达）时在此补发一次失败完成事件，便于前端提示。
    if let Err(e) = &r {
        let _ = app.emit(
            "file-pull-complete",
            serde_json::json!({
                "transfer_id": pull_id,
                "ok": false,
                "error": e.clone(),
            }),
        );
    }
    r
}
