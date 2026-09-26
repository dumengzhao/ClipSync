//! 自写轻量更新器（无签名自托管模型，见 server/UPDATE_MODULE_PLAN.md 第 6 节）。
//!
//! - 更新基址来自用户配置的 `server_url`（relay 地址）的 https origin —— 绝不硬编码作者服务器。
//! - `check_update`：拉 `<base>/update/latest.json`（公开、无鉴权）→ 与当前版本比对。
//! - `download_update`：流式下载到临时目录 + 计算 sha256，与 manifest 比对（完整性校验）。
//! - `install_update`：按平台拉起安装包（Windows 弹出 NSIS 交互向导，随后退出进程交给安装器）。
//! - 无签名：信任锚 = 用户自己的中继服务器 + TLS。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tauri::{Emitter, Manager, State};

use crate::AppState;

/// 下载进度事件（emit 到前端 `update-progress`）。
///
/// 下载此前完全静默，用户点了「下载并安装」后界面毫无反应，会误以为程序卡死或已退出。
/// 这里把阶段与百分比回传，前端据此显示进度。
#[derive(Debug, Clone, Serialize)]
pub struct DownloadProgress {
    /// `downloading` / `verifying` / `done` / `error`
    pub phase: String,
    pub downloaded: u64,
    pub total: u64,
    /// 0-100；`total` 未知时为 None
    pub percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

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

/// GitHub 直连更新的来源仓库。
///
/// 与「从服务端中继更新」是**两条互不影响的独立链路**（用户明确要求不纠缠）：
///
/// | | 清单与安装包来源 | 信任锚 |
/// |---|---|---|
/// | 中继链路（原有） | 用户自己配置的 `server_url`（同源校验） | 用户自己的服务器 + TLS |
/// | GitHub 链路（本模块） | 本仓库的 Release（`releases/latest`） | **github.com 的 TLS** + 仓库完整性 |
///
/// 两条链路**共用同一套安装闸门**：`download_update_impl` 里落盘前的 sha256 校验、
/// 以及 `install_verified_update` 里「只认本次下载并校验通过的那个包」+ 启动前复算哈希。
/// 也就是说 GitHub 链路并不会放宽任何安装侧的安全约束。
pub const GITHUB_REPO: &str = "dumengzhao/ClipSync";

/// GitHub 链路允许下载的域名：仓库本体 + Release 资产的实际落点
/// （`github.com` 会 302 到 CDN，reqwest 自动跟随）。
///
/// 仍然要校验主机：清单虽然来自 github.com，但万一它被替换（或仓库被投毒），
/// 也不能借我们之手去下载任意外域的二进制。
const GITHUB_ALLOWED_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "github-releases.githubusercontent.com",
];

/// 本仓库最新发布清单的地址。
///
/// 用 `releases/latest` 资产路径而**不是** GitHub API：资产路径天然排除草稿与预发布，
/// 与「人工 Publish 才对外可见」的发布流程一致，且不吃 API 的 60 次/小时限额。
pub fn github_manifest_url() -> String {
    format!("https://github.com/{GITHUB_REPO}/releases/latest/download/latest.json")
}

/// 从用户配置的 server_url 推导更新基址。
///
/// 规则：**配的什么就用什么**（用户明确要求不因安全边界而拒绝）——
/// `wss://host/ws` → `https://host`、`ws://host/ws` → `http://host`、
/// `https://host` → `https://host`、`http://host` → `http://host`；
/// 无协议前缀时按 `ws` 兜底（与 relay 端默认一致）→ `http://host`。
pub fn update_base_from_server_url(server_url: &str) -> Option<String> {
    let s = server_url.trim();
    if s.is_empty() {
        return None;
    }
    let (tls, rest) = if let Some(r) = s.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = s.strip_prefix("ws://") {
        (false, r)
    } else if let Some(r) = s.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = s.strip_prefix("http://") {
        (false, r)
    } else {
        // 无协议前缀：按 ws 兜底（relay 端 server_url 常省略为 host:port）
        (false, s)
    };
    let authority = rest.split(['/', '?']).next()?.trim();
    if authority.is_empty() {
        return None;
    }
    Some(format!(
        "{}://{authority}",
        if tls { "https" } else { "http" }
    ))
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

/// 当前进程是否为「安装版」（经 NSIS 安装包装到系统）。
///
/// 判定依据（按优先级）：
/// 1. 当前 exe 同目录存在 NSIS 安装钩子写入的 `installed.marker` —— 这是首选，
///    因为它专门为此目的设计，不会被其他软件/用户误删也不会被同名文件误命中。
/// 2. fallback：同目录存在 `uninstall.exe`（兼容 0.1.0 及更早未带 marker 的版本）。
///
/// 选用 marker 文件而不是单靠 uninstall.exe 的原因：用户可能把绿色版 exe
/// 拷到任意目录运行——任何目录里都不会有这两个文件，判定必然是「绿色版」。
/// 反过来真正的安装版必然经过 NSIS 安装流程，marker 必然存在。
///
/// 两个豁免（不影响正式使用）：
/// - `debug_assertions`（debug 构建）恒为 true：开发/自测需要能触达更新流程；
/// - 环境变量 `CLIPSYNC_FORCE_UPDATE` 存在时恒为 true：release 绿色版临时自测用。
///
/// **macOS 单独判定**：上面两条都是 Windows/NSIS 专属（`installed.marker` 由 NSIS
/// 钩子写入），在 mac 上恒不成立，会把更新入口（托盘「检查更新」、设置页、下载/安装）
/// 整个关死。故 macOS 改为识别标准 bundle：`exe` 位于 `X.app/Contents/MacOS/` 下即放行。
/// 依据：mac 的更新只是 `open` 下载好的 dmg 引导用户拖拽，**不会改写正在运行的 app**，
/// 不存在 Windows「免安装版被悄悄变成安装版」的风险，无需照搬那道门禁。
pub fn is_installed_build() -> bool {
    if std::env::var("CLIPSYNC_FORCE_UPDATE").is_ok() {
        return true;
    }
    if cfg!(debug_assertions) {
        return true;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    // macOS：运行在 .app bundle 内即视为正式安装版（详见上方文档注释）。
    #[cfg(target_os = "macos")]
    if is_in_app_bundle(&exe) {
        return true;
    }
    let Some(dir) = exe.parent() else {
        return false;
    };
    // marker 是首选（更可靠），uninstall.exe 作为 fallback（兼容旧版本）
    if dir.join("installed.marker").exists() {
        return true;
    }
    if dir.join("uninstall.exe").exists() {
        return true;
    }
    false
}

/// 判断给定可执行文件路径是否位于 macOS 标准 app bundle 内，
/// 形如 `.../ClipSync.app/Contents/MacOS/clipsync`。
///
/// 抽成独立纯函数便于单测——`is_installed_build()` 依赖 `current_exe()`，不好直接测。
#[cfg(target_os = "macos")]
fn is_in_app_bundle(exe: &std::path::Path) -> bool {
    let Some(dir) = exe.parent() else {
        return false;
    };
    dir.file_name()
        .map(|n| n == std::ffi::OsStr::new("MacOS"))
        .unwrap_or(false)
        && dir
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n == std::ffi::OsStr::new("Contents"))
            .unwrap_or(false)
        && dir
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().ends_with(".app"))
            .unwrap_or(false)
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
    do_check_update(&server_url).await
}

/// 前端启动时调用一次，判断是否显示更新相关 UI。
/// `false` = 当前是免安装版（直接双击 exe），更新链路不可用。
#[tauri::command]
pub fn is_installed_build_cmd() -> bool {
    is_installed_build()
}

/// 核心检查逻辑（与 tauri command 解耦，供托盘菜单等无需 `State` 的调用方复用）。
pub async fn do_check_update(server_url: &str) -> Result<Option<UpdateInfo>, String> {
    let base = update_base_from_server_url(server_url)
        .ok_or_else(|| "未配置服务端地址，请在设置里填写服务端连接地址".to_string())?;
    fetch_manifest(&format!("{base}/update/latest.json")).await
}

/// 直连 GitHub 检查更新（设置页「检查更新 GitHub」按钮）。
///
/// 与中继链路完全独立：**不读 `server_url`**，所以服务端没配 / 连不上 / 没发布清单时，
/// 这条链路依然可用。
#[tauri::command]
pub async fn check_update_github() -> Result<Option<UpdateInfo>, String> {
    fetch_manifest(&github_manifest_url()).await
}

/// 拉取并解析更新清单 → 与当前版本比对。
/// `Ok(None)` = 已是最新，或该地址下还没有清单（404，例如对方尚未发布过）。
pub async fn fetch_manifest(url: &str) -> Result<Option<UpdateInfo>, String> {
    let resp = reqwest::get(url)
        .await
        .map_err(|e| format!("请求更新清单失败: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("更新清单返回 HTTP {}", resp.status()));
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
        .ok_or_else(|| format!("更新清单缺少本平台（{}）安装包", platform_key()))?;
    Ok(Some(UpdateInfo {
        version: m.version,
        notes: m.notes,
        pub_date: m.pub_date,
        url: entry.url,
        sha256: entry.sha256,
    }))
}

/// 下载来源。决定 URL 白名单规则 —— 这是「渲染器可传任意 URL」这个信任边界上的关键一环。
enum DownloadSource {
    /// 中继链路：必须与本机配置的服务端**同源**（scheme + host + port）。
    /// 清单是服务端下发的：若不校验，服务端被控或响应被篡改时就能把安装包指向
    /// 任意外域（乃至明文 http），而 SHA256 也来自同一份清单、形同虚设。
    Relay,
    /// GitHub 链路：必须 https，且主机在 `GITHUB_ALLOWED_HOSTS` 内。
    Github,
}

/// GitHub 链路的主机/协议校验。
///
/// 抽成纯函数是为了能单测 —— 这是「渲染器可传任意 URL」边界上的一条安全规则，
/// 值得钉死。用**主机名精确匹配**而不是 `contains`/后缀判断：后者会被
/// `github.com.evil.tld`、`evilgithub.com` 这类名字骗过。
fn github_url_allowed(target: &reqwest::Url) -> Result<(), String> {
    if target.scheme() != "https" {
        return Err("GitHub 更新地址必须是 https".to_string());
    }
    let host = target.host_str().unwrap_or("");
    if !GITHUB_ALLOWED_HOSTS.contains(&host) {
        return Err(format!("更新地址不在 GitHub 允许的域名内（{host}）"));
    }
    Ok(())
}

/// 下载安装包到临时目录，流式计算 sha256 并与 manifest 比对；
/// 不一致则删除文件并报错。成功返回本地路径。**中继链路入口。**
#[tauri::command]
pub async fn download_update(
    app: tauri::AppHandle,
    url: String,
    sha256: String,
) -> Result<String, String> {
    download_update_impl(app, url, sha256, DownloadSource::Relay).await
}

/// 同上下载，但走 **GitHub 链路**（设置页「检查更新 GitHub」）。
#[tauri::command]
pub async fn download_update_github(
    app: tauri::AppHandle,
    url: String,
    sha256: String,
) -> Result<String, String> {
    download_update_impl(app, url, sha256, DownloadSource::Github).await
}

/// 两条链路共用的下载实现。**除来源校验外逐行一致** ——
/// 也就是说 GitHub 链路不会放宽任何一条既有约束：随机临时目录 + `create_new` 防预置、
/// 流式 sha256、校验失败即删、只有校验通过才登记 `pending_update`（安装闸门见
/// `install_verified_update`）。
async fn download_update_impl(
    app: tauri::AppHandle,
    url: String,
    sha256: String,
    source: DownloadSource,
) -> Result<String, String> {
    if !is_installed_build() {
        return Err("当前为免安装版，不支持在线更新（请使用 NSIS 安装版）".to_string());
    }
    let target = reqwest::Url::parse(&url).map_err(|e| format!("更新地址非法: {e}"))?;
    match source {
        DownloadSource::Relay => {
            let expected_base = {
                let state = app.state::<crate::AppState>();
                let server_url = state.config.lock().server_url.clone();
                update_base_from_server_url(&server_url)
            };
            let expected =
                expected_base.ok_or_else(|| "未配置服务端地址，无法校验更新来源".to_string())?;
            let allowed =
                reqwest::Url::parse(&expected).map_err(|e| format!("服务端地址非法: {e}"))?;
            if target.scheme() != allowed.scheme()
                || target.host_str() != allowed.host_str()
                || target.port_or_known_default() != allowed.port_or_known_default()
            {
                return Err(format!(
                    "更新地址与配置的服务端不同源，已拒绝下载：{url}（期望源自 {expected}）"
                ));
            }
        }
        DownloadSource::Github => {
            if let Err(why) = github_url_allowed(&target) {
                return Err(format!("{why}，已拒绝下载：{url}"));
            }
        }
    }
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
    // 总大小用于算百分比；服务端未给 Content-Length 时为 0 → 前端只显示已下载字节数。
    let total: u64 = resp.content_length().unwrap_or(0);
    // 每次下载使用**独占的随机目录**。此前是固定的 `temp/clipsync-update` +
    // 可预测文件名，本机其它进程可以预置同名文件/符号链接，配合「先校验后安装」
    // 的时序做替换（TOCTOU）。
    let dir: PathBuf =
        std::env::temp_dir().join(format!("clipsync-update-{}", uuid::Uuid::new_v4().simple()));
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("创建临时目录失败: {e}"))?;
    // 顺手清理历史遗留的下载目录（本次之外的 clipsync-update*），避免残留堆积。
    // **只删「足够旧」的目录**：如果无条件删除，同一时刻的另一次下载（用户重试、
    // 或另一个窗口）刚创建的临时目录会被这次清理顺手删掉——Windows 上因目录内含
    // 打开的文件而删不掉才侥幸无事，Linux 上文件被 unlink 后安装阶段就找不到包了。
    // 本进程刚创建的目录 mtime 必然很新，按 mtime 过滤即可彻底避免误删。
    const STALE_DOWNLOAD_DIR_AGE: std::time::Duration = std::time::Duration::from_secs(3600);
    if let Ok(mut rd) = tokio::fs::read_dir(std::env::temp_dir()).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let n = entry.file_name().to_string_lossy().into_owned();
            if !n.starts_with("clipsync-update") || entry.path() == dir {
                continue;
            }
            let is_stale = entry
                .metadata()
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > STALE_DOWNLOAD_DIR_AGE);
            if is_stale {
                let _ = tokio::fs::remove_dir_all(entry.path()).await;
            }
        }
    }
    let path = dir.join(&fname);
    let tmp = dir.join(format!("{fname}.download"));
    // create_new：不存在才创建，绝不跟随/覆盖既有文件
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)
        .await
        .map_err(|e| format!("创建临时文件失败: {e}"))?;
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;
    // 进度回传（-5 保证首次 0% 一定 emit）
    let mut last_pct: i64 = -5;
    let emit = |phase: &str, downloaded: u64, msg: Option<String>| {
        // total 为 0（服务端未给 Content-Length）时 checked_div 自然得到 None，无需另判
        let percent = downloaded
            .checked_mul(100)
            .and_then(|v| v.checked_div(total))
            .map(|p| p.min(100) as u8);
        let _ = app.emit(
            "update-progress",
            DownloadProgress {
                phase: phase.to_string(),
                downloaded,
                total,
                percent,
                message: msg,
            },
        );
    };
    emit("downloading", 0, None);
    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = stream.next().await {
        let c = chunk.map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("下载中断: {e}")
        })?;
        hasher.update(&c);
        size += c.len() as u64;
        if let Err(e) = file.write_all(&c).await {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("写入临时文件失败: {e}"));
        }
        // 节流：每变化 5% 才 emit 一次，避免高频事件刷爆前端
        if let Some(pct) = size
            .checked_mul(100)
            .and_then(|v| v.checked_div(total))
            .map(|p| p.min(100) as i64)
        {
            if pct - last_pct >= 5 || size == total {
                last_pct = pct;
                emit("downloading", size, None);
            }
        }
    }
    if let Err(e) = file.sync_all().await {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("落盘失败: {e}"));
    }
    drop(file);
    emit("verifying", size, Some("正在校验文件完整性…".to_string()));
    let got = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if got != sha256.trim().to_lowercase() {
        let _ = std::fs::remove_file(&tmp);
        let msg = format!("sha256 校验失败（期望 {sha256}，实际 {got}）——已删除下载文件");
        emit("error", size, Some(msg.clone()));
        return Err(msg);
    }
    if let Err(e) = tokio::fs::rename(&tmp, &path).await {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("重命名失败: {e}"));
    }
    // 登记「本次下载且校验通过」的安装包：install_update 只认这条记录，
    // 避免该命令被用来拉起任意本地程序（参数来自前端渲染器）。
    {
        let state = app.state::<crate::AppState>();
        *state.pending_update.lock() = Some((path.clone(), got.clone()));
    }
    emit(
        "done",
        size,
        Some("下载完成，正在启动安装程序…".to_string()),
    );
    Ok(path.to_string_lossy().into_owned())
}

/// 运行安装包。Windows：无参数交互模式弹出 NSIS 向导（用户确认后安装、装完自动启动新版本），
/// 随后退出当前进程，避免安装时文件被占用。
/// macOS：open 引导用户；Linux：AppImage 加执行位后拉起 / deb 走 pkexec dpkg -i。
#[tauri::command]
pub async fn install_update(
    state: tauri::State<'_, crate::AppState>,
    path: String,
) -> Result<(), String> {
    install_verified_update(&state, std::path::Path::new(&path)).await
}

/// 安装的核心实现（命令与托盘菜单共用）。
///
/// **只允许安装本进程本次下载并校验通过的那个包**：命令参数由渲染器传入，
/// 若不与 `AppState::pending_update` 绑定，脚本注入即可 `install_update(path=任意 exe)`
/// 拉起任意程序（紧接着还会 exit 掉主进程）——那等价于一个代码执行入口。
pub async fn install_verified_update(
    state: &crate::AppState,
    path: &std::path::Path,
) -> Result<(), String> {
    if !is_installed_build() {
        return Err("当前为免安装版，不支持在线更新（请使用 NSIS 安装版）".to_string());
    }
    let p = path.to_path_buf();
    let expected = {
        let guard = state.pending_update.lock();
        match guard.as_ref() {
            Some(v) => v.clone(),
            None => return Err("没有待安装的更新包（请先在本界面完成下载）".to_string()),
        }
    };
    if expected.0 != p {
        return Err("拒绝安装：路径与本次下载的更新包不一致".to_string());
    }
    if !p.exists() {
        return Err(format!("安装包不存在: {}", p.display()));
    }
    // 启动前再核对一次哈希：下载→安装之间文件可能被替换（同机低权限进程、
    // 杀软隔离后重写），只在下完时校验一次不足以防这个 TOCTOU 窗口。
    {
        let mut hasher = Sha256::new();
        let mut f = std::fs::File::open(&p).map_err(|e| format!("打开安装包失败: {e}"))?;
        std::io::copy(&mut f, &mut hasher).map_err(|e| format!("读取安装包失败: {e}"))?;
        let got = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        if got != expected.1 {
            return Err("拒绝安装：安装包内容与下载校验结果不一致（可能已被替换）".to_string());
        }
    }
    // Windows：先退出当前进程再由安装器接管，避免文件占用
    #[cfg(target_os = "windows")]
    {
        // 不要给 NSIS 传 `/P`：实测该参数不被本安装包识别，安装器启动后**立刻自行结束**——
        // 不显示向导、装完也不启动应用，用户只看到主程序凭空退出（像是更新失败）。
        // 无参数即交互模式：NSIS 正常弹出安装向导，用户确认后安装，装完由安装器自动启动新版本。
        let child = std::process::Command::new(&p)
            .spawn()
            .map_err(|e| format!("启动安装器失败: {e}"))?;
        tracing::info!(
            "更新安装器已启动（pid {}），退出当前进程交给安装器接管",
            child.id()
        );
        // 给安装器一点时间完成进程初始化：父进程立即 exit 时子进程可能还没真正起来。
        std::thread::sleep(std::time::Duration::from_millis(300));
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
        let s = p.to_string_lossy().into_owned();
        let s = s.as_str();
        if s.ends_with(".AppImage") {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&p)
                .map_err(|e| format!("读取元数据失败: {e}"))?
                .permissions();
            perm.set_mode(perm.mode() | 0o755);
            std::fs::set_permissions(&p, perm).map_err(|e| format!("设置执行位失败: {e}"))?;
            std::process::Command::new(&p)
                .spawn()
                .map_err(|e| format!("启动 AppImage 失败: {e}"))?;
            Ok(())
        } else if s.ends_with(".deb") {
            std::process::Command::new("pkexec")
                .args(["dpkg", "-i", s])
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
    fn base_from_server_url_uses_configured_scheme() {
        // 配的什么就用什么：wss→https
        assert_eq!(
            update_base_from_server_url("wss://sync.example.com/ws"),
            Some("https://sync.example.com".into())
        );
        assert_eq!(
            update_base_from_server_url("https://a.com:8443"),
            Some("https://a.com:8443".into())
        );
        // ws→http、http→http（用户明确要求不因安全边界而拒绝明文）
        assert_eq!(
            update_base_from_server_url("ws://sync.example.com/ws"),
            Some("http://sync.example.com".into())
        );
        assert_eq!(
            update_base_from_server_url("http://a.com"),
            Some("http://a.com".into())
        );
        // 本机地址走 ws 也映射为 http
        assert_eq!(
            update_base_from_server_url("ws://127.0.0.1:20075/ws"),
            Some("http://127.0.0.1:20075".into())
        );
        // 无协议前缀按 ws 兜底 → http
        assert_eq!(
            update_base_from_server_url("host:20070"),
            Some("http://host:20070".into())
        );
        assert_eq!(
            update_base_from_server_url("127.0.0.1:20070"),
            Some("http://127.0.0.1:20070".into())
        );
        // 空值
        assert_eq!(update_base_from_server_url(""), None);
        assert_eq!(update_base_from_server_url("   "), None);
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

    /// GitHub 链路的下载地址白名单：**精确主机匹配 + 强制 https**。
    /// 这条规则挡的是「清单被替换后把安装包指向任意外域」，必须钉死。
    #[test]
    fn github_download_url_policy() {
        let ok = |u: &str| github_url_allowed(&reqwest::Url::parse(u).unwrap()).is_ok();

        // 允许：仓库本体 + 资产 CDN 实际落点
        assert!(ok(
            "https://github.com/dumengzhao/ClipSync/releases/download/v1/a.exe"
        ));
        assert!(ok("https://release-assets.githubusercontent.com/a/b"));
        assert!(ok("https://objects.githubusercontent.com/a/b"));

        // 拒绝：明文 http
        assert!(!ok(
            "http://github.com/dumengzhao/ClipSync/releases/download/v1/a.exe"
        ));

        // 拒绝：其它域名
        assert!(!ok("https://evil.tld/a.exe"));
        assert!(!ok(
            "https://raw.githubusercontent.com/dumengzhao/ClipSync/main/a.exe"
        ));

        // 关键：看像但不是的，不能被子串/后缀判断骗过（必须是精确主机名相等）
        assert!(!ok("https://github.com.evil.tld/a.exe"));
        assert!(!ok("https://evilgithub.com/a.exe"));
        assert!(!ok("https://notgithub.com/a.exe"));
        assert!(!ok("https://githubusercontent.com/a.exe"));
    }

    /// 清单地址必须是 `releases/latest` 的**资产**路径：
    /// 该路径天然排除草稿与预发布（与「人工 Publish 才对外可见」的发布流程一致）。
    #[test]
    fn github_manifest_url_uses_assets_path() {
        let u = github_manifest_url();
        assert_eq!(
            u,
            format!("https://github.com/{GITHUB_REPO}/releases/latest/download/latest.json")
        );
        assert!(u.ends_with("/releases/latest/download/latest.json"));
    }

    /// `is_installed_build` 的核心判定逻辑（剥离豁免分支，跑真实文件检测）。
    /// 返回 true 仅当同目录存在 installed.marker 或 uninstall.exe。
    fn detect_installed_in(dir: &std::path::Path) -> bool {
        if dir.join("installed.marker").exists() {
            return true;
        }
        if dir.join("uninstall.exe").exists() {
            return true;
        }
        false
    }

    #[test]
    fn installed_marker_is_required_green_version_anywhere() {
        // 临时目录 1：纯 exe 拷贝（无 marker / 无 uninstall.exe）→ 绿色版
        let tmp = std::env::temp_dir().join("clipsync-test-green");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("clipsync.exe"), b"fake-exe").unwrap();
        assert!(
            !detect_installed_in(&tmp),
            "无 marker 也无 uninstall.exe → 必须判定为绿色版（用户拷 exe 到任意目录的场景）"
        );
        std::fs::remove_dir_all(&tmp).unwrap();

        // 临时目录 2：模拟 NSIS 装的目录（marker 存在）→ 安装版
        let tmp2 = std::env::temp_dir().join("clipsync-test-installed-marker");
        let _ = std::fs::remove_dir_all(&tmp2);
        std::fs::create_dir_all(&tmp2).unwrap();
        std::fs::write(tmp2.join("clipsync.exe"), b"fake-exe").unwrap();
        std::fs::write(tmp2.join("installed.marker"), b"ClipSync installed build\n").unwrap();
        assert!(
            detect_installed_in(&tmp2),
            "有 installed.marker → 判定安装版（NSIS 新装路径）"
        );
        std::fs::remove_dir_all(&tmp2).unwrap();

        // 临时目录 3：模拟老版本安装目录（无 marker 但有 uninstall.exe）→ fallback 判定安装版
        let tmp3 = std::env::temp_dir().join("clipsync-test-installed-fallback");
        let _ = std::fs::remove_dir_all(&tmp3);
        std::fs::create_dir_all(&tmp3).unwrap();
        std::fs::write(tmp3.join("clipsync.exe"), b"fake-exe").unwrap();
        std::fs::write(tmp3.join("uninstall.exe"), b"fake-uninst").unwrap();
        assert!(
            detect_installed_in(&tmp3),
            "无 marker 但有 uninstall.exe → fallback 判定安装版（兼容 0.1.0 旧版）"
        );
        std::fs::remove_dir_all(&tmp3).unwrap();

        // 临时目录 4：uninstall.exe 存在但被改名为别的后缀（用户改名/清理残留）→ 不算安装版
        // 这是「修复你担心的判断逻辑」的核心场景：mark 检测能穿透 uninstall.exe 缺失的情况。
        let tmp4 = std::env::temp_dir().join("clipsync-test-renamed");
        let _ = std::fs::remove_dir_all(&tmp4);
        std::fs::create_dir_all(&tmp4).unwrap();
        std::fs::write(tmp4.join("clipsync.exe"), b"fake-exe").unwrap();
        std::fs::write(tmp4.join("uninstall.exe.bak"), b"backup").unwrap(); // 不算 uninstall.exe
        assert!(
            !detect_installed_in(&tmp4),
            "只有 uninstall.exe.bak → 判定绿色版（说明 marker 缺失时也不能靠同名文件误判）"
        );
        std::fs::remove_dir_all(&tmp4).unwrap();
    }

    /// macOS bundle 结构识别：位于 `X.app/Contents/MacOS/` 下才算正式 .app。
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_bundle_path_is_recognized() {
        assert!(
            is_in_app_bundle(std::path::Path::new(
                "/Applications/ClipSync.app/Contents/MacOS/clipsync"
            )),
            "标准安装路径 /Applications 必须识别为 bundle"
        );
        assert!(
            is_in_app_bundle(std::path::Path::new(
                "/Users/dmz/Applications/ClipSync.app/Contents/MacOS/clipsync"
            )),
            "用户级 ~/Applications 安装也必须识别"
        );
        assert!(
            !is_in_app_bundle(std::path::Path::new("/tmp/clipsync")),
            "裸可执行文件不算 bundle"
        );
        assert!(
            !is_in_app_bundle(std::path::Path::new("/tmp/ClipSync.app/MacOS/clipsync")),
            "缺少 Contents 层不算标准 bundle"
        );
    }
}
