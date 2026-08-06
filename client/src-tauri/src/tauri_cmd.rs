//! Tauri 命令定义
//!
//! 前端通过 `invoke()` 调用这些命令

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::clipboard::types::ClipboardContent;
use crate::clipboard::{ClipboardProvider, PlatformClipboard};
use crate::config::settings::ManualAddress;
use crate::device::registry::{PairedDevice, TrustLevel};
use crate::discovery::DiscoveredPeer;
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
    // 配对码可能变更，热更新到传输中枢（无需重启即对新连接生效）
    state.hub.set_pairing_code(cfg.pairing_code.clone());

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

/// 列出当前通过 mDNS 发现的局域网对端
#[tauri::command]
pub fn list_discovered_peers(state: State<AppState>) -> Vec<DiscoveredPeer> {
    state.discovered.lock().values().cloned().collect()
}

#[tauri::command]
pub async fn get_clipboard() -> Result<String, String> {
    let cb = PlatformClipboard::new();
    match cb.read().await {
        Ok(ClipboardContent::Text(t)) => Ok(t),
        Ok(_) => Err("clipboard does not contain text".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn set_clipboard(text: String) -> Result<(), String> {
    let cb = PlatformClipboard::new();
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
}

impl From<PairedDevice> for DeviceInfo {
    fn from(p: PairedDevice) -> Self {
        Self {
            id: p.device_id.0,
            name: p.device_name,
            fingerprint: p.fingerprint,
            trusted: matches!(p.trust, TrustLevel::Verified),
            last_seen: p.last_seen,
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
