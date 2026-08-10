//! Tauri 命令定义
//!
//! 前端通过 `invoke()` 调用这些命令

use serde::Serialize;
use tauri::{AppHandle, State};

use crate::clipboard::types::ClipboardContent;
use crate::clipboard::ClipboardProvider;
use crate::config::settings::ManualAddress;
use crate::device::registry::{PairedDevice, TrustLevel};
use crate::discovery::DiscoveredPeer;
use crate::discovery::manual::ManualAddressBook;
use crate::AppState;

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

    crate::config::save_config(&app, &cfg).map_err(|e| e.to_string())?;
    *state.config.lock() = cfg.clone();

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
    #[cfg(debug_assertions)]
    {
        // dev 模式下额外窗口加载前端不可靠（白屏），改为通知主窗口内嵌显示设置视图
        app.emit("open-settings", ()).map_err(|e| e.to_string())
    }
    #[cfg(not(debug_assertions))]
    {
        // 正式构建：创建独立设置窗口
        if let Some(w) = tauri::Manager::get_webview_window(&app, "settings") {
            let _ = w.show();
            let _ = w.set_focus();
            return Ok(());
        }
        let url = tauri::WebviewUrl::App("/".into());
        tauri::WebviewWindowBuilder::new(&app, "settings", url)
            .title("ClipSync 设置")
            .inner_size(760.0, 600.0)
            .min_inner_size(560.0, 460.0)
            .resizable(true)
            .center()
            .build()
            .map_err(|e| e.to_string())?;
        Ok(())
    }
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

/// 生成 6 位随机配对码并「武装」本端监听（应答方角色）。
/// 返回该码供前端展示给用户，由用户线下告知对方。
#[tauri::command]
pub fn generate_pairing_code(state: State<AppState>) -> String {
    state.hub.generate_pairing_code()
}

/// 取消当前武装的配对码。
#[tauri::command]
pub fn cancel_pairing(state: State<AppState>) {
    state.hub.cancel_pairing();
}

/// 返回当前武装中的配对码（若有），供前端刷新时恢复展示。
#[tauri::command]
pub fn get_pending_pairing(state: State<AppState>) -> Option<String> {
    state.hub.pending_pairing_code()
}

/// 用户在前端手动发起配对：作为发起方，使用输入的配对码连接指定对端。
///
/// 对端必须已「生成配对码」并展示同一码（处于武装状态），否则握手会被拒绝。
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
