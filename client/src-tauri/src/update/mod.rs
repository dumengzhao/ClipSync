//! 自写轻量更新器（无签名自托管模型，见 server/UPDATE_MODULE_PLAN.md 第 6 节）。
//!
//! - 更新基址来自用户配置的 `server_url`（relay 地址）的 https origin —— 绝不硬编码作者服务器。
//! - `check_update`：拉 `<base>/update/latest.json`（公开、无鉴权）→ 与当前版本比对。
//! - `download_update`：流式下载到临时目录 + 计算 sha256，与 manifest 比对（完整性校验）。
//! - `install_update`：按平台拉起安装包（Windows NSIS 被动模式后退出进程交给安装器）。
//! - 无签名：信任锚 = 用户自己的中继服务器 + TLS。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tauri::State;

use crate::AppState;

/// 服务端自定义 latest.json（无 signature / 无 pubkey）。
#[derive(Debug, Deserialize)]
pub struct LatestManifest {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub pub_date: String,
    pub platforms: std::collections::BTreeMap<String, PlatformEntry>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PlatformEntry {
    pub url: String,
    pub sha256: String,
}

/// 传给前端的更新信息。
#[derive(Debug, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub pub_date: String,
    pub url: String,
    pub sha256: String,
}

/// 本平台在 manifest `platforms` 里的键（与 server/src/update.rs 的 PLATFORMS 白名单一致）。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub fn platform_key() -> &'static str {
    "windows-x86_64"
}
#[cfg(all(target_os = "windows", target_arch = "aarch64"))]
pub fn platform_key() -> &'static str {
    "windows-aarch64"
}
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub fn platform_key() -> &'static str {
    "darwin-x86_64"
}
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn platform_key() -> &'static str {
    "darwin-aarch64"
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn platform_key() -> &'static str {
    "linux-x86_64"
}
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
pub fn platform_key() -> &'static str {
    "linux-aarch64"
}
#[cfg(not(any(
    all(target_os = "windows", target_arch = "x86_64"),
    all(target_os = "windows", target_arch = "aarch64"),
    all(target_os = "macos", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64"),
)))]
pub fn platform_key() -> &'static str {
    "unknown"
}

/// 从用户配置的 server_url 推导更新基址。
///
/// 接受 `wss://host:port/ws` / `https://host` （映射为 `https://host:port`）；
/// `ws://` / `http://` 一律拒绝（TLS 是无签名模型唯一安全边界），
/// 仅豁免本机回环（127.0.0.1 / localhost / [::1]）供开发联调。
pub fn update_base_from_server_url(server_url: &str) -> Option<String> {
    let s = server_url.trim();
    let (scheme_is_tls, rest) = if let Some(r) = s.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = s.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = s.strip_prefix("ws://") {
        (false, r)
    } else if let Some(r) = s.strip_prefix("http://") {
        (false, r)
    } else {
        return None;
    };
    let authority = rest.split(['/', '?']).next()?.trim();
    if authority.is_empty() {
        return None;
    }
    let host = match authority.rsplit_once(':') {
        // IPv6 字面量 [::1]:port
        Some((h, _)) if h.starts_with('[') => h,
        Some((h, _)) => h,
        None => authority,
    };
    let host = host.trim_end_matches(']');
    let is_loopback = matches!(host, "127.0.0.1" | "localhost" | "::1");
    if scheme_is_tls || is_loopback {
        Some(format!(
            "{}://{authority}",
            if scheme_is_tls { "https" } else { "http" }
        ))
    } else {
        None
    }
}

/// 语义化版本粗比较：按 `主.次.补丁` 数值逐段比较，manifest 更新才返回 true。
/// 任一侧解析失败则退化为「字符串不同即有更新」。
pub fn is_newer(manifest_version: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> {
        let core = v.split(['-', '+']).next()?.trim();
        let parts: Vec<u64> = core
            .split('.')
            .map(|p| p.trim().parse().ok())
            .collect::<Option<_>>()?;
        if parts.is_empty() {
            None
        } else {
            Some(parts)
        }
    };
    match (parse(manifest_version), parse(current)) {
        (Some(a), Some(b)) => {
            for i in 0..a.len().max(b.len()) {
                let x = a.get(i).copied().unwrap_or(0);
                let y = b.get(i).copied().unwrap_or(0);
                if x != y {
                    return x > y;
                }
            }
            false
        }
        _ => manifest_version != current,
    }
}

fn basename_of(url: &str) -> String {
    let s = url.trim_end_matches(['/', '\\']);
    match s.rsplit(['/', '\\']).next() {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => String::new(),
    }
}

/// GET `<base>/update/latest.json` → 与当前版本比对。
/// 返回 `Ok(None)` 表示「已是最新 / 服务端未发布（404）」。
#[tauri::command]
pub async fn check_update(state: State<'_, AppState>) -> Result<Option<UpdateInfo>, String> {
    let server_url = state.config.lock().server_url.clone();
    let base = update_base_from_server_url(&server_url).ok_or_else(|| {
        "未配置 wss:// 服务端地址（更新基址仅接受 https）".to_string()
    })?;
    let url = format!("{base}/update/latest.json");
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("请求更新清单失败: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("服务端返回 HTTP {}", resp.status()));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取更新清单失败: {e}"))?;
    let m: LatestManifest =
        serde_json::from_str(&text).map_err(|e| format!("更新清单解析失败: {e}"))?;
    let current = env!("CARGO_PKG_VERSION");
    if !is_newer(&m.version, current) {
        return Ok(None);
    }
    let entry = m
        .platforms
        .get(platform_key())
        .cloned()
        .ok_or_else(|| format!("服务端清单缺少本平台（{}）安装包", platform_key()))?;
    Ok(Some(UpdateInfo {
        version: m.version,
        notes: m.notes,
        pub_date: m.pub_date,
        url: entry.url,
        sha256: entry.sha256,
    }))
}

/// 下载安装包到临时目录，流式计算 sha256 并与 manifest 比对；
/// 不一致则删除文件并报错。成功返回本地路径。
#[tauri::command]
pub async fn download_update(url: String, sha256: String) -> Result<String, String> {
    let fname = basename_of(&url);
    if fname.is_empty() || fname.contains("..") || fname.contains('/') || fname.contains('\\') {
        return Err(format!("无效的下载文件名: {fname}"));
    }
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("下载请求失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载返回 HTTP {}", resp.status()));
    }
    let dir: PathBuf = std::env::temp_dir().join("clipsync-update");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("创建临时目录失败: {e}"))?;
    let path = dir.join(&fname);
    let tmp = dir.join(format!("{fname}.download"));
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| format!("创建临时文件失败: {e}"))?;
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;
    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = stream.next().await {
        let c = chunk.map_err(|e| {
            let _ = tokio::fs::remove_file(&tmp);
            format!("下载中断: {e}")
        })?;
        hasher.update(&c);
        size += c.len() as u64;
        if let Err(e) = file.write_all(&c).await {
            let _ = tokio::fs::remove_file(&tmp);
            return Err(format!("写入临时文件失败: {e}"));
        }
    }
    if let Err(e) = file.sync_all().await {
        let _ = tokio::fs::remove_file(&tmp);
        return Err(format!("落盘失败: {e}"));
    }
    drop(file);
    let got = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if got != sha256.trim().to_lowercase() {
        let _ = tokio::fs::remove_file(&tmp);
        return Err(format!(
            "sha256 校验失败（期望 {sha256}，实际 {got}）——已删除下载文件"
        ));
    }
    if let Err(e) = tokio::fs::rename(&tmp, &path).await {
        let _ = tokio::fs::remove_file(&tmp);
        return Err(format!("重命名失败: {e}"));
    }
    let _ = size;
    Ok(path.to_string_lossy().into_owned())
}

/// 运行安装包。Windows：NSIS 被动模式（/P 显示进度免交互）并退出当前进程；
/// macOS：open 引导用户；Linux：AppImage 加执行位后拉起 / deb 走 pkexec dpkg -i。
#[tauri::command]
pub async fn install_update(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err(format!("安装包不存在: {path}"));
    }
    // Windows：先退出当前进程再由安装器接管，避免文件占用
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new(&p)
            .arg("/P") // NSIS 被动模式：显示进度条、无需交互（Tauri NSIS 支持 /S 静默 /P 被动）
            .spawn()
            .map_err(|e| format!("启动安装器失败: {e}"))?;
        std::process::exit(0);
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&p)
            .spawn()
            .map_err(|e| format!("打开安装包失败: {e}"))?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let s = path.to_string_lossy().into_owned();
        if s.ends_with(".AppImage") {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&p)
                .map_err(|e| format!("读取元数据失败: {e}"))?
                .permissions();
            perm.set_mode(perm.mode() | 0o755);
            std::fs::set_permissions(&p, perm)
                .map_err(|e| format!("设置执行位失败: {e}"))?;
            std::process::Command::new(&p)
                .spawn()
                .map_err(|e| format!("启动 AppImage 失败: {e}"))?;
            Ok(())
        } else if s.ends_with(".deb") {
            std::process::Command::new("pkexec")
                .args(["dpkg", "-i", &s])
                .spawn()
                .map_err(|e| format!("启动 dpkg 失败: {e}"))?;
            Ok(())
        } else {
            std::process::Command::new("xdg-open")
                .arg(&p)
                .spawn()
                .map_err(|e| format!("打开文件失败: {e}"))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_https_only_with_loopback_exempt() {
        assert_eq!(
            update_base_from_server_url("wss://sync.example.com/ws"),
            Some("https://sync.example.com".into())
        );
        assert_eq!(
            update_base_from_server_url("https://a.com:8443"),
            Some("https://a.com:8443".into())
        );
        // 明文 ws/http → 拒绝（安全边界）
        assert_eq!(update_base_from_server_url("ws://sync.example.com/ws"), None);
        assert_eq!(update_base_from_server_url("http://a.com"), None);
        // 本机回环豁免（开发联调）
        assert_eq!(
            update_base_from_server_url("ws://127.0.0.1:20075/ws"),
            Some("http://127.0.0.1:20075".into())
        );
        assert_eq!(
            update_base_from_server_url("ws://localhost:20075/ws"),
            Some("http://localhost:20075".into())
        );
        // 空值/垃圾输入
        assert_eq!(update_base_from_server_url(""), None);
        assert_eq!(update_base_from_server_url("host:20070"), None);
    }

    #[test]
    fn version_compare() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.2", "0.1.5"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        // 解析失败 → 退化为字符串比较
        assert!(is_newer("beta-2", "0.1.0"));
    }
}
