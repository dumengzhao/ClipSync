//! 客户端更新托管模块（无签名自托管模型，见 server/UPDATE_MODULE_PLAN.md）。
//!
//! - 公开读：`GET /update/latest.json`（url 按本机 origin/UPDATE_PUBLIC_BASE 改写）、
//!   `GET /update/files/:platform/:file`（整文件流式返回）、
//!   `GET /update/versions.json` 与 `GET /downloads`（公开下载页，见本文件「公开下载页」一节）。
//! - 管理：`GET /api/admin/update` 当前版本摘要、`GET /api/admin/update/history` 历史版本、
//!   `POST /api/admin/update/downloads` 公开历史开关、`POST /api/admin/update` multipart 上传
//!   （字段顺序约定：每组文件前先发 `platform`、`filename` 文本字段，再发 `file`）。
//! - 信任模型：无签名，完整性校验靠 manifest 的 sha256；来源真伪由 TLS + 服务器保证。
//! - 落盘沿用 storage.rs 套路：写 `*.tmp` 再 rename 原子替换，避免半截文件被拉走。

use crate::state::AppState;
use axum::body::Body;
use axum::extract::{Multipart, Path, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

/// 平台白名单：manifest 键与上传目标目录都必须命中，防任意目录写入。
pub const PLATFORMS: &[&str] = &[
    "windows-x86_64",
    "windows-aarch64",
    "darwin-x86_64",
    "darwin-aarch64",
    "linux-x86_64",
    "linux-aarch64",
];

/// 单个 manifest 文本字段大小上限（latest.json 很小，1MB 足够）。
const MANIFEST_MAX_BYTES: u64 = 1024 * 1024;
/// 文件名长度上限。
const FILENAME_MAX_LEN: usize = 200;

#[derive(Debug, Deserialize, Serialize)]
pub struct UpdateManifest {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub pub_date: String,
    pub platforms: std::collections::BTreeMap<String, PlatformEntry>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PlatformEntry {
    pub url: String,
    pub sha256: String,
}

// ---------- 纯函数（可单测） ----------

/// 从 url/路径取最后一段文件名（兼容 `/` 与 `\`）。
pub fn basename_of(url: &str) -> String {
    let s = url.trim_end_matches(['/', '\\']);
    match s.rsplit(['/', '\\']).next() {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => String::new(),
    }
}

/// 文件名安全校验：非空、无路径分隔符、无 `..`、无控制字符、长度受限。
/// 不满足即拒绝（防目录穿越）。
pub fn is_safe_filename(name: &str) -> bool {
    if name.is_empty() || name.len() > FILENAME_MAX_LEN {
        return false;
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return false;
    }
    if name.chars().any(|c| c.is_control() || c == '\0') {
        return false;
    }
    true
}

/// 平台键合法性。
pub fn is_valid_platform(p: &str) -> bool {
    PLATFORMS.contains(&p)
}

/// 计算改写用的公开基址：
/// 1) 优先 `UPDATE_PUBLIC_BASE`（去掉尾部 `/`）；
/// 2) 否则请求头 `X-Forwarded-Proto: https`（nginx 反代注入）拼 Host；
/// 3) 否则 `Origin` 头本身为 `https://...` 时用 Origin；
/// 4) 都没有 → Err（**仅接受 https**：TLS 是无签名模型唯一安全边界，
///    宁可 500 也不生成 http 更新链接）。
pub fn effective_base(state: &AppState, headers: &HeaderMap) -> Result<String, String> {
    if let Some(b) = &state.update_public_base {
        let b = b.trim_end_matches('/').to_string();
        if !b.is_empty() {
            return Ok(b);
        }
    }
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if proto == "https" && !host.is_empty() {
        return Ok(format!("https://{host}"));
    }
    if let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        if origin.starts_with("https://") {
            return Ok(origin.trim_end_matches('/').to_string());
        }
    }
    Err("cannot determine https base for update urls: set UPDATE_PUBLIC_BASE (e.g. https://sync.example.com)".to_string())
}

/// 把 manifest 中各平台 url 改写为 `<base>/update/files/<platform>/<basename>`。
pub fn rewrite_urls(manifest: &mut Value, base: &str) {
    let Some(platforms) = manifest
        .get_mut("platforms")
        .and_then(|p| p.as_object_mut())
    else {
        return;
    };
    for (platform, entry) in platforms.iter_mut() {
        let Some(url) = entry.get("url").and_then(|u| u.as_str()) else {
            continue;
        };
        let name = basename_of(url);
        if name.is_empty() {
            continue;
        }
        if let Some(obj) = entry.as_object_mut() {
            obj.insert(
                "url".to_string(),
                Value::String(format!("{base}/update/files/{platform}/{name}")),
            );
        }
    }
}

/// 校验 manifest 结构：version 非空、platforms 非空、键在白名单内、每项含 url。
///
/// **`sha256` 允许为空**：哈希由服务端在落盘时自行计算并覆盖（见 `admin_upload`）。
/// 早先要求前端提供，导致管理页必须用浏览器 Web Crypto —— 而 `crypto.subtle` 只在
/// 安全上下文（https / localhost）存在，用 `http://<公网IP>/admin` 打开时直接报
/// 「当前环境不支持 Web Crypto」，发布功能完全不可用。
/// 由服务端算还有个好处：哈希必然对应**实际存盘的那份字节**，比前端算的更权威。
pub fn validate_manifest(raw: &str) -> Result<UpdateManifest, String> {
    let m: UpdateManifest =
        serde_json::from_str(raw).map_err(|e| format!("manifest invalid: {e}"))?;
    if m.version.trim().is_empty() {
        return Err("manifest.version required".into());
    }
    if m.platforms.is_empty() {
        return Err("manifest.platforms required".into());
    }
    for (p, e) in &m.platforms {
        if !is_valid_platform(p) {
            return Err(format!("unknown platform: {p}"));
        }
        if e.url.trim().is_empty() {
            return Err(format!("platform {p}: url required"));
        }
    }
    Ok(m)
}

/// 合并策略（服务端 latest.json 是**持久累积**文件，各平台可独立发布）：
/// - 顶层 `version`：以本次上传为准（发布方负责改版本号）；
/// - `notes` / `pub_date`：本次上传非空才覆盖，留空则沿用线上值——
///   便于「只传某个平台的包、不改发布说明」；
/// - `platforms`：按平台键逐条合并，本次上传涉及的键覆盖/新增，
///   其它平台原样保留（Windows 发包不会抹掉 darwin 条目）。
pub fn merge_manifest(
    existing: Option<UpdateManifest>,
    incoming: UpdateManifest,
) -> UpdateManifest {
    let Some(mut base) = existing else {
        return incoming;
    };
    base.version = incoming.version;
    if !incoming.notes.trim().is_empty() {
        base.notes = incoming.notes;
    }
    if !incoming.pub_date.trim().is_empty() {
        base.pub_date = incoming.pub_date;
    }
    for (p, e) in incoming.platforms {
        base.platforms.insert(p, e);
    }
    base
}

fn files_root(state: &AppState) -> PathBuf {
    state.update_dir.join("files")
}

// ---------- 公开端点 ----------

/// GET /update/latest.json —— 公开读；url 按本机基址改写后返回。
pub async fn latest_json(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let path = state.update_dir.join("latest.json");
    // 用异步读：handler 跑在 tokio worker 上，同步文件 IO 会阻塞该线程
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(s) => s,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                [(CONTENT_TYPE, "application/json")],
                json!({"error": "no update published"}).to_string(),
            )
                .into_response()
        }
    };
    let mut v: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(CONTENT_TYPE, "application/json")],
                json!({"error": format!("manifest invalid: {e}")}).to_string(),
            )
                .into_response()
        }
    };
    let base = match effective_base(&state, &headers) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(CONTENT_TYPE, "application/json")],
                json!({"error": e}).to_string(),
            )
                .into_response()
        }
    };
    rewrite_urls(&mut v, &base);
    ([(CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

/// GET /update/files/:platform/:file —— 公开读，流式整文件返回。
pub async fn download_file(
    State(state): State<Arc<AppState>>,
    Path((platform, file)): Path<(String, String)>,
) -> Response {
    if !is_valid_platform(&platform) || !is_safe_filename(&file) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let path = files_root(&state).join(&platform).join(&file);
    let f = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    let size = f.metadata().await.map(|m| m.len()).unwrap_or(0);
    let stream = tokio_util::io::ReaderStream::new(f);
    let body = Body::from_stream(stream);
    let mut resp = (
        [
            (CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                CONTENT_DISPOSITION,
                format!("attachment; filename=\"{file}\""),
            ),
        ],
        body,
    )
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_LENGTH,
        size.to_string().parse().unwrap(),
    );
    resp
}

// ---------- 公开下载页（无需登录） ----------

/// 文件里没有版本号时归到这个组（公开页会把它过滤掉，管理页留着便于排查）。
const UNKNOWN_VERSION: &str = "未识别版本";

/// 公开下载页的开关，落盘在 `<update_dir>/downloads.json`。
///
/// 默认**只公开当前线上版本**：数据目录里往往还躺着测试包、中途失败的版本，
/// 一开历史就等于把它们也挂到公网。要公开历史得管理员显式打开（见管理页开关）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadsConfig {
    #[serde(default)]
    pub public_history: bool,
}

fn downloads_config_path(update_dir: &std::path::Path) -> std::path::PathBuf {
    update_dir.join("downloads.json")
}

/// 读开关：文件不存在/读不动一律当**关闭**（失败要往安全一侧倒）。
async fn load_downloads_config(update_dir: &std::path::Path) -> DownloadsConfig {
    tokio::fs::read_to_string(downloads_config_path(update_dir))
        .await
        .ok()
        .and_then(|s| serde_json::from_str::<DownloadsConfig>(&s).ok())
        .unwrap_or_default()
}

fn save_downloads_config(
    update_dir: &std::path::Path,
    cfg: &DownloadsConfig,
) -> Result<(), String> {
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    crate::storage::atomic_write(&downloads_config_path(update_dir), &text)
        .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
pub struct PublicHistoryBody {
    pub enabled: bool,
}

/// POST /api/admin/update/downloads —— 切换公开下载页是否列出历史版本（需登录）。
pub async fn admin_set_downloads(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PublicHistoryBody>,
) -> Response {
    let cfg = DownloadsConfig {
        public_history: body.enabled,
    };
    match save_downloads_config(&state.update_dir, &cfg) {
        Ok(()) => {
            tracing::info!(
                "公开下载页：历史版本{}",
                if cfg.public_history {
                    "已公开（所有历史包任何人都能下载）"
                } else {
                    "只公开当前线上版本"
                }
            );
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "public_history": cfg.public_history })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("写入 downloads.json 失败：{e}") })),
        )
            .into_response(),
    }
}

/// GET /update/versions.json —— **公开**（无鉴权）：给下载页用的版本数据。
///
/// 只包含「公开页面敢给人看」的东西：版本号、更新时间、更新说明、包名/大小/时间。
/// 不含服务器路径、凭据或任何管理信息。历史版本是否包含取决于管理页的开关。
///
/// 下载链接一律用**相对路径**（`/update/files/...`），让浏览器按当前 host 解析 ——
/// 这样同一个端点在局域网 HTTP、公网 HTTPS、反代域名下都对，服务端不需要知道自己的外网地址。
pub async fn public_versions(State(state): State<Arc<AppState>>) -> Response {
    let current = current_version_of(&state).await;
    let cfg = load_downloads_config(&state.update_dir).await;

    // 当前版本：以 latest.json 为准（notes / pub_date 只有这里有）
    let mut platforms = serde_json::Map::new();
    let (mut pub_date, mut notes) = (String::new(), String::new());
    if let Ok(raw) = tokio::fs::read_to_string(state.update_dir.join("latest.json")).await {
        if let Ok(m) = serde_json::from_str::<UpdateManifest>(&raw) {
            pub_date = m.pub_date;
            notes = m.notes;
            for (p, e) in &m.platforms {
                let name = basename_of(&e.url);
                let fp = files_root(&state).join(p).join(&name);
                let (size, ts) = match std::fs::metadata(&fp) {
                    Ok(md) => (
                        Some(md.len()),
                        md.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs()),
                    ),
                    Err(_) => (None, None),
                };
                platforms.insert(
                    p.clone(),
                    json!({
                        "filename": name,
                        "size": size,
                        "updated_at": ts,
                        // 文件还没上传时不给链接，免得点了 404
                        "available": size.is_some(),
                    }),
                );
            }
        }
    }

    // 历史：开关打开时才给（排除当前版本，也排除认不出版本的组）。
    // 扫描目录只在真要给出历史时才做 —— 这是个**匿名**端点，别让关掉开关的部署
    // 每个请求都白扫一遍文件系统。
    let history: Vec<Value> = if cfg.public_history {
        scan_version_groups(&state)
            .await
            .iter()
            .filter(|(v, _, _)| v != UNKNOWN_VERSION && current.as_deref() != Some(v.as_str()))
            .map(|(v, ts, files)| json!({ "version": v, "updated_at": ts, "files": files }))
            .collect()
    } else {
        Vec::new()
    };

    (
        [(CONTENT_TYPE, "application/json")],
        json!({
            "version": current,
            "pub_date": pub_date,
            "notes": notes,
            "platforms": platforms,
            "public_history": cfg.public_history,
            "history": history,
        })
        .to_string(),
    )
        .into_response()
}

/// GET /downloads —— 公开的下载页面（内嵌静态资源，无登录、无管理入口）。
pub async fn downloads_page() -> Response {
    match crate::admin::Assets::get("downloads.html") {
        Some(f) => (
            StatusCode::OK,
            [(CONTENT_TYPE, "text/html; charset=utf-8")],
            f.data.to_vec(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "downloads page not embedded".to_string()).into_response(),
    }
}

// ---------- 管理端点（admin_auth 之下） ----------

/// 从安装包文件名里猜出版本号：`ClipSync_0.3.2_x64-setup.exe` → `0.3.2`。
///
/// 判据是「**至少三段**用点分隔的数字」（`dots >= 2`），这样
/// `x86_64`（没有点）、`amd64.deb`（点后面不是数字）、`5.10`（只有两段）都不会被误当成版本；
/// 平台后缀（`aarch64`/`amd64`/`x86_64`/`.deb`/`.rpm`）因此天然被排除。
/// 取第一个命中的：版本号在发行包命名里总是出现在中间那一段。
fn parse_version_from_name(name: &str) -> Option<String> {
    let cs: Vec<char> = name.chars().collect();
    let mut i = 0usize;
    while i < cs.len() {
        if !cs[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut dots = 0usize;
        while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.') {
            if cs[i] == '.' {
                dots += 1;
            }
            i += 1;
        }
        // 数字串后面紧跟的点不算（`1.0.0.deb`、`64.`）
        let mut end = i;
        while end > start && cs[end - 1] == '.' {
            end -= 1;
            dots -= 1;
        }
        if dots >= 2 && end > start {
            return Some(cs[start..end].iter().collect());
        }
    }
    None
}

/// 读当前线上版本号（没有 latest.json 或读不动 = None）。
async fn current_version_of(state: &AppState) -> Option<String> {
    let raw = tokio::fs::read_to_string(state.update_dir.join("latest.json"))
        .await
        .ok()?;
    serde_json::from_str::<UpdateManifest>(&raw).ok().map(|m| m.version)
}

/// 版本号排序键：按点切段转成数字（`0.10.0` > `0.9.0`，字符串比较会搞反）。
/// 解析不出来的（“未识别版本”）得到 `[0]`，自然排到最后。
fn version_sort_key(v: &str) -> Vec<u64> {
    v.split('.').map(|s| s.parse::<u64>().unwrap_or(0)).collect()
}

/// GET /api/admin/update/history —— **所有历史上的安装包**，按版本分组。
///
/// 历史是自然存在的：包按 `files/<platform>/<文件名>` 落盘，而文件名里带版本号，
/// 新版本不会覆盖旧版本 —— 所以这个端点不需要额外记账，直接扫目录即可。
/// 手动上传的包与「从 GitHub 同步」下来的包都在同一个目录里，一并在列。
/// 扫描 `<update_dir>/files/*/`，按版本分组。
///
/// 返回 `(版本, 该版本最新落盘时刻, 该版本的文件行)`，**已排好序**：
/// 组间按版本号降序（新版在上，同版本按落盘时间倒序），组内按平台名。
/// 管理页的「历史版本」与公开下载页共用这一份扫描结果 —— 两边的差异只在**过滤**。
async fn scan_version_groups(state: &AppState) -> Vec<(String, u64, Vec<Value>)> {
    let current = current_version_of(state).await;
    let root = files_root(state);
    // version -> (该版本最新的落盘时刻, 文件行)
    let mut groups: std::collections::BTreeMap<String, (u64, Vec<Value>)> =
        std::collections::BTreeMap::new();

    if let Ok(mut platforms) = tokio::fs::read_dir(&root).await {
        while let Ok(Some(entry)) = platforms.next_entry().await {
            let platform = entry.file_name().to_string_lossy().to_string();
            if !is_valid_platform(&platform) {
                continue; // 目录里的其它东西（latest.json 等）与平台目录无关
            }
            let Ok(mut files) = tokio::fs::read_dir(entry.path()).await else {
                continue;
            };
            while let Ok(Some(f)) = files.next_entry().await {
                let name = f.file_name().to_string_lossy().to_string();
                // 上传中断可能留下 .tmp 半成品
                if name.ends_with(".tmp") || !is_safe_filename(&name) {
                    continue;
                }
                let Ok(md) = f.metadata().await else { continue };
                if !md.is_file() {
                    continue;
                }
                let ts = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let version =
                    parse_version_from_name(&name).unwrap_or_else(|| UNKNOWN_VERSION.to_string());
                let is_current = current.as_deref() == Some(version.as_str());
                let slot = groups.entry(version).or_insert((0, Vec::new()));
                slot.0 = slot.0.max(ts);
                slot.1.push(json!({
                    "platform": platform,
                    "filename": name,
                    "size": md.len(),
                    "updated_at": ts,
                    "is_current": is_current,
                }));
            }
        }
    }

    let mut versions: Vec<(String, u64, Vec<Value>)> = groups
        .into_iter()
        .map(|(v, (ts, mut files))| {
            files.sort_by_key(|f| {
                f.get("platform")
                    .and_then(|p| p.as_str())
                    .unwrap_or("")
                    .to_string()
            });
            (v, ts, files)
        })
        .collect();
    versions.sort_by(|a, b| {
        version_sort_key(&b.0)
            .cmp(&version_sort_key(&a.0))
            .then(b.1.cmp(&a.1))
    });
    versions
}

/// GET /api/admin/update/history —— **所有历史上的安装包**，按版本分组。
///
/// 历史是自然存在的：包按 `files/<platform>/<文件名>` 落盘，而文件名里带版本号，
/// 新版本不会覆盖旧版本 —— 所以这个端点不需要额外记账，直接扫目录即可。
/// 手动上传的包与「从 GitHub 同步」下来的包都在同一个目录里，一并在列。
pub async fn admin_history(State(state): State<Arc<AppState>>) -> Response {
    let current = current_version_of(&state).await;
    let groups = scan_version_groups(&state).await;
    let total: usize = groups.iter().map(|(_, _, f)| f.len()).sum();
    let out: Vec<Value> = groups
        .into_iter()
        .map(|(v, ts, files)| {
            json!({
                "version": v,
                "updated_at": ts,
                "is_current": current.as_deref() == Some(v.as_str()),
                "files": files,
            })
        })
        .collect();

    (
        [(CONTENT_TYPE, "application/json")],
        json!({
            "current_version": current,
            "total_files": total,
            "versions": out,
            // 公开下载页是否列出历史（管理页的开关状态回显）
            "public_history": load_downloads_config(&state.update_dir).await.public_history,
        })
        .to_string(),
    )
        .into_response()
}

/// GET /api/admin/update —— 当前线上版本摘要（无发布则 404）。
pub async fn admin_info(State(state): State<Arc<AppState>>) -> Response {
    let path = state.update_dir.join("latest.json");
    // 用异步读：这些 handler 跑在 tokio worker 上，同步文件 IO 会阻塞该线程
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(s) => s,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                [(CONTENT_TYPE, "application/json")],
                json!({"error": "no update published"}).to_string(),
            )
                .into_response()
        }
    };
    let m: UpdateManifest = match serde_json::from_str(&raw) {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(CONTENT_TYPE, "application/json")],
                json!({"error": format!("manifest invalid: {e}")}).to_string(),
            )
                .into_response()
        }
    };
    let mut platforms = serde_json::Map::new();
    for (p, e) in &m.platforms {
        let name = basename_of(&e.url);
        let fp = files_root(&state).join(p).join(&name);
        // (是否已上传, 字节数, 落盘时刻 epoch 秒）—— 管理页要按平台逐行展示「包名 / 大小 / 日期」
        let (uploaded, size, updated_at) = match std::fs::metadata(&fp) {
            Ok(md) => (
                true,
                md.len(),
                md.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
            ),
            Err(_) => (false, 0, None),
        };
        platforms.insert(
            p.clone(),
            json!({
                "filename": name,
                "sha256": e.sha256,
                "uploaded": uploaded,
                "size": if uploaded { Value::from(size) } else { Value::Null },
                "updated_at": updated_at,
            }),
        );
    }
    (
        [(CONTENT_TYPE, "application/json")],
        json!({
            "version": m.version,
            "pub_date": m.pub_date,
            "notes": m.notes,
            "platforms": platforms,
        })
        .to_string(),
    )
        .into_response()
}

/// POST /api/admin/update —— multipart 上传 latest.json + 安装包。
/// 字段顺序约定（前端保证）：每组文件先 `platform`、`filename` 文本字段，随后 `file`。
pub async fn admin_upload(State(state): State<Arc<AppState>>, mut mp: Multipart) -> Response {
    let max_total = state.update_max_upload;
    let err = |code: StatusCode, msg: String| async move {
        (
            code,
            [(CONTENT_TYPE, "application/json")],
            json!({"error": msg}).to_string(),
        )
            .into_response() as Response
    };

    let mut manifest_raw: Option<String> = None;
    let mut pending_platform: Option<String> = None;
    let mut pending_filename: Option<String> = None;
    // (platform, filename, size)
    // (platform, filename, size, sha256_hex)
    let mut uploaded: Vec<(String, String, u64, String)> = Vec::new();
    let mut total: u64 = 0;

    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return err(StatusCode::BAD_REQUEST, format!("multipart error: {e}")).await,
        };
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "platform" => {
                pending_platform = match field.text().await {
                    Ok(t) => Some(t.trim().to_string()),
                    Err(e) => {
                        return err(StatusCode::BAD_REQUEST, format!("platform field: {e}")).await
                    }
                };
            }
            "filename" => {
                pending_filename = match field.text().await {
                    Ok(t) => Some(t.trim().to_string()),
                    Err(e) => {
                        return err(StatusCode::BAD_REQUEST, format!("filename field: {e}")).await
                    }
                };
            }
            "manifest" => {
                // 限量读文本
                let mut buf: Vec<u8> = Vec::new();
                let mut f = field;
                loop {
                    match f.chunk().await {
                        Ok(Some(c)) => {
                            if buf.len() as u64 + c.len() as u64 > MANIFEST_MAX_BYTES {
                                return err(
                                    StatusCode::PAYLOAD_TOO_LARGE,
                                    "manifest too large".into(),
                                )
                                .await;
                            }
                            buf.extend_from_slice(&c);
                        }
                        Ok(None) => break,
                        Err(e) => {
                            return err(StatusCode::BAD_REQUEST, format!("manifest field: {e}"))
                                .await
                        }
                    }
                }
                match String::from_utf8(buf) {
                    Ok(s) => manifest_raw = Some(s),
                    Err(_) => {
                        return err(StatusCode::BAD_REQUEST, "manifest must be utf-8".into()).await
                    }
                }
            }
            "file" => {
                let platform = match pending_platform.take() {
                    Some(p) if !p.is_empty() => p,
                    _ => {
                        return err(
                            StatusCode::BAD_REQUEST,
                            "file part requires preceding platform field".into(),
                        )
                        .await
                    }
                };
                if !is_valid_platform(&platform) {
                    return err(
                        StatusCode::BAD_REQUEST,
                        format!("unknown platform: {platform}"),
                    )
                    .await;
                }
                let filename = match pending_filename.take() {
                    Some(f) if !f.is_empty() => f,
                    // 容错：未显式给 filename 时用 part 自带 file_name
                    _ => match field.file_name() {
                        Some(f) if !f.is_empty() => f.to_string(),
                        _ => {
                            return err(
                                StatusCode::BAD_REQUEST,
                                "file part requires filename field".into(),
                            )
                            .await
                        }
                    },
                };
                if !is_safe_filename(&filename) {
                    return err(
                        StatusCode::BAD_REQUEST,
                        format!("unsafe filename: {filename}"),
                    )
                    .await;
                }
                let dir = files_root(&state).join(&platform);
                if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                    return err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("create dir: {e}"),
                    )
                    .await;
                }
                let tmp = dir.join(format!("{filename}.tmp"));
                let dst = dir.join(&filename);
                let mut out = match tokio::fs::File::create(&tmp).await {
                    Ok(o) => o,
                    Err(e) => {
                        return err(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("create file: {e}"),
                        )
                        .await
                    }
                };
                let mut size: u64 = 0;
                // 边收边算 SHA-256：哈希必须对应**实际落盘的那份字节**，
                // 因此由服务端算，不再依赖前端（浏览器 Web Crypto 仅安全上下文可用）。
                let mut hasher = Sha256::new();
                let mut f = field;
                loop {
                    match f.chunk().await {
                        Ok(Some(c)) => {
                            size += c.len() as u64;
                            total += c.len() as u64;
                            hasher.update(&c);
                            if total > max_total {
                                drop(out);
                                let _ = tokio::fs::remove_file(&tmp).await;
                                return err(
                                    StatusCode::PAYLOAD_TOO_LARGE,
                                    format!(
                                        "upload exceeds UPDATE_MAX_UPLOAD_MB limit ({max_total} bytes)"
                                    ),
                                )
                                .await;
                            }
                            use tokio::io::AsyncWriteExt;
                            if let Err(e) = out.write_all(&c).await {
                                let _ = tokio::fs::remove_file(&tmp).await;
                                return err(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    format!("write file: {e}"),
                                )
                                .await;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            let _ = tokio::fs::remove_file(&tmp).await;
                            return err(StatusCode::BAD_REQUEST, format!("file stream: {e}")).await;
                        }
                    }
                }
                if let Err(e) = out.sync_all().await {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return err(StatusCode::INTERNAL_SERVER_ERROR, format!("sync file: {e}")).await;
                }
                drop(out);
                if let Err(e) = tokio::fs::rename(&tmp, &dst).await {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("rename file: {e}"),
                    )
                    .await;
                }
                uploaded.push((platform, filename, size, hex::encode(hasher.finalize())));
            }
            _ => {
                // 未知字段：读掉避免连接挂起
                let mut f = field;
                while let Ok(Some(_)) = f.chunk().await {}
            }
        }
    }

    // 落 manifest（最后原子替换，保证「传齐再换」）
    let raw = match manifest_raw {
        Some(r) => r,
        None => {
            return err(StatusCode::BAD_REQUEST, "manifest field required".into()).await;
        }
    };
    let incoming = match validate_manifest(&raw) {
        Ok(m) => m,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).await,
    };

    // 与线上已有的 latest.json 合并：本次上传只覆盖自己涉及的平台条目与版本信息，
    // 其余平台保留。服务端 latest.json 因此成为「固定不动、按平台累积」的持久文件。
    let latest_dst = state.update_dir.join("latest.json");
    let existing: Option<UpdateManifest> = tokio::fs::read_to_string(&latest_dst)
        .await
        .ok()
        // 现有文件坏了：以本次上传为准重建，不让一次解析失败卡死发布
        .and_then(|s| serde_json::from_str::<UpdateManifest>(&s).ok());
    let previous_version = existing.as_ref().map(|e| e.version.clone());
    let merged_from_existing = existing.is_some();
    let mut m = merge_manifest(existing, incoming);
    // 用服务端自算的哈希覆盖清单里对应平台的 sha256：
    // 前端传来的值一律不作数（浏览器端可能算错，也可能根本算不了——见 validate_manifest 注释）。
    for (platform, _filename, _size, digest) in &uploaded {
        if let Some(entry) = m.platforms.get_mut(platform) {
            entry.sha256 = digest.clone();
        }
    }

    let pretty = match serde_json::to_string_pretty(&m) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("encode: {e}")).await,
    };
    if let Err(e) = tokio::fs::create_dir_all(&state.update_dir).await {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create update dir: {e}"),
        )
        .await;
    }
    let latest_tmp = state.update_dir.join("latest.json.tmp");
    if let Err(e) = tokio::fs::write(&latest_tmp, pretty).await {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write manifest: {e}"),
        )
        .await;
    }
    if let Err(e) = tokio::fs::rename(&latest_tmp, &latest_dst).await {
        let _ = tokio::fs::remove_file(&latest_tmp).await;
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("rename manifest: {e}"),
        )
        .await;
    }

    let platforms: Vec<&String> = m.platforms.keys().collect();
    (
        [(CONTENT_TYPE, "application/json")],
        json!({
            "ok": true,
            "version": m.version,
            "previous_version": previous_version,
            "merged": merged_from_existing,
            "platforms": platforms,
            "uploaded": uploaded.iter().map(|(p, f, s, _h)| json!({
                "platform": p, "filename": f, "size": s
            })).collect::<Vec<_>>(),
        })
        .to_string(),
    )
        .into_response()
}

// ---------- 测试 ----------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_handles_separators() {
        assert_eq!(basename_of("https://a.com/x/y/Setup.exe"), "Setup.exe");
        assert_eq!(basename_of("file.exe"), "file.exe");
        assert_eq!(basename_of("/a/b\\c.deb"), "c.deb");
        assert_eq!(basename_of("/"), "");
        assert_eq!(basename_of(""), "");
    }

    #[test]
    fn safe_filename_rejects_traversal() {
        assert!(is_safe_filename("ClipSync_0.1.1_x64-setup.exe"));
        assert!(is_safe_filename("app.AppImage"));
        assert!(!is_safe_filename("../secret"));
        assert!(!is_safe_filename("a/b"));
        assert!(!is_safe_filename("a\\b"));
        assert!(!is_safe_filename(".."));
        assert!(!is_safe_filename(""));
        assert!(!is_safe_filename(&"x".repeat(FILENAME_MAX_LEN + 1)));
    }

    #[test]
    fn manifest_validation() {
        let ok =
            r#"{"version":"0.1.1","platforms":{"windows-x86_64":{"url":"a.exe","sha256":"aa"}}}"#;
        assert!(validate_manifest(ok).is_ok());
        let bad_platform =
            r#"{"version":"0.1.1","platforms":{"etc/passwd":{"url":"a","sha256":"aa"}}}"#;
        assert!(validate_manifest(bad_platform).is_err());
        // sha256 允许为空：哈希由服务端落盘时计算并覆盖（浏览器 Web Crypto 仅安全上下文可用）
        let no_sha =
            r#"{"version":"0.1.1","platforms":{"windows-x86_64":{"url":"a.exe","sha256":""}}}"#;
        assert!(validate_manifest(no_sha).is_ok());
        // url 仍必须提供
        let no_url = r#"{"version":"0.1.1","platforms":{"windows-x86_64":{"url":"","sha256":""}}}"#;
        assert!(validate_manifest(no_url).is_err());
        let no_version =
            r#"{"version":"","platforms":{"windows-x86_64":{"url":"a.exe","sha256":"aa"}}}"#;
        assert!(validate_manifest(no_version).is_err());
    }

    fn manifest(
        ver: &str,
        notes: &str,
        pub_date: &str,
        entries: &[(&str, &str, &str)],
    ) -> UpdateManifest {
        UpdateManifest {
            version: ver.into(),
            notes: notes.into(),
            pub_date: pub_date.into(),
            platforms: entries
                .iter()
                .map(|(p, u, s)| {
                    (
                        p.to_string(),
                        PlatformEntry {
                            url: u.to_string(),
                            sha256: s.to_string(),
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn merge_without_existing_returns_incoming() {
        let m = manifest("0.1.1", "n", "d", &[("windows-x86_64", "a.exe", "aa")]);
        let out = merge_manifest(None, m);
        assert_eq!(out.version, "0.1.1");
        assert_eq!(out.platforms.len(), 1);
    }

    #[test]
    fn merge_keeps_other_platforms_and_overrides_same_key() {
        let existing = manifest(
            "0.1.0",
            "old notes",
            "2026-09-01T00:00:00Z",
            &[
                ("windows-x86_64", "Setup-0.1.0.exe", "old-win"),
                ("darwin-aarch64", "ClipSync-0.1.0.dmg", "old-mac"),
            ],
        );
        // Windows 单独发 0.1.1（manifest 里只有自己那一条）
        let incoming = manifest(
            "0.1.1",
            "win notes",
            "2026-09-02T00:00:00Z",
            &[("windows-x86_64", "Setup-0.1.1.exe", "new-win")],
        );
        let out = merge_manifest(Some(existing), incoming);
        assert_eq!(out.version, "0.1.1", "版本以本次上传为准");
        assert_eq!(out.notes, "win notes");
        assert_eq!(out.pub_date, "2026-09-02T00:00:00Z");
        assert_eq!(out.platforms.len(), 2, "darwin 条目必须保留");
        assert_eq!(out.platforms["windows-x86_64"].sha256, "new-win");
        assert_eq!(
            out.platforms["darwin-aarch64"].url, "ClipSync-0.1.0.dmg",
            "未涉及的平台原样保留"
        );
    }

    #[test]
    fn merge_keeps_notes_when_incoming_blank() {
        let existing = manifest(
            "0.1.0",
            "keep me",
            "2026-09-01T00:00:00Z",
            &[("linux-x86_64", "a.AppImage", "aa")],
        );
        let incoming = manifest("0.1.0", "   ", "", &[("linux-x86_64", "b.AppImage", "bb")]);
        let out = merge_manifest(Some(existing), incoming);
        assert_eq!(out.notes, "keep me", "留空不覆盖已有描述");
        assert_eq!(out.pub_date, "2026-09-01T00:00:00Z", "留空不覆盖已有日期");
        assert_eq!(out.platforms["linux-x86_64"].sha256, "bb");
    }

    #[test]
    fn rewrite_urls_uses_basename() {
        let mut v: Value = serde_json::from_str(
            r#"{"version":"0.1.1","platforms":{"windows-x86_64":{"url":"https://old/ClipSync_0.1.1_x64-setup.exe","sha256":"aa"}}}"#,
        )
        .unwrap();
        rewrite_urls(&mut v, "https://sync.example.com");
        let url = v["platforms"]["windows-x86_64"]["url"].as_str().unwrap();
        assert_eq!(
            url,
            "https://sync.example.com/update/files/windows-x86_64/ClipSync_0.1.1_x64-setup.exe"
        );
    }

    #[test]
    fn base_requires_https() {
        let state = test_state(None);
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::HOST,
            "sync.example.com".parse().unwrap(),
        );
        // 无 https 依据 → 拒绝
        assert!(effective_base(&state, &h).is_err());
        // X-Forwarded-Proto: https → 接受
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        assert_eq!(
            effective_base(&state, &h).unwrap(),
            "https://sync.example.com"
        );
        // https Origin 也可
        let mut h2 = HeaderMap::new();
        h2.insert(
            axum::http::header::ORIGIN,
            "https://relay.example.com".parse().unwrap(),
        );
        assert_eq!(
            effective_base(&state, &h2).unwrap(),
            "https://relay.example.com"
        );
        // UPDATE_PUBLIC_BASE 优先
        let state2 = test_state(Some("https://cdn.example.com/"));
        assert_eq!(
            effective_base(&state2, &HeaderMap::new()).unwrap(),
            "https://cdn.example.com"
        );
    }

    fn test_state(public_base: Option<&str>) -> AppState {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("clipsync-ut-{}-{uniq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        AppState {
            store: crate::storage::Store::new(dir.clone()),
            networks: std::sync::Mutex::new(vec![]),
            hub: crate::hub::Hub::new(),
            admin_ws: std::sync::Mutex::new(std::collections::HashMap::new()),
            server_key: "test-key".into(),
            admin_creds: std::sync::Mutex::new(Some(crate::storage::AdminCreds {
                user: "admin".into(),
                pass_hash: crate::storage::hash_pass("pw"),
                updated_at: 0,
            })),
            update_dir: dir.join("update"),
            update_public_base: public_base.map(|s| s.to_string()),
            update_max_upload: 10 * 1024 * 1024,
            trusted_proxies: vec![],
        }
    }

    // ---- 集成测试：走完整路由（上传 → 摘要 → 公开读 → 下载 → 401/404） ----

    use axum::http::Request;
    use tower::ServiceExt; // oneshot

    fn multipart_body(
        boundary: &str,
        platform: &str,
        filename: &str,
        manifest: &str,
        file_bytes: &[u8],
    ) -> (axum::http::HeaderValue, Vec<u8>) {
        let mut b = Vec::new();
        let push = |b: &mut Vec<u8>, s: &str| b.extend_from_slice(s.as_bytes());
        push(&mut b, &format!("--{boundary}\r\n"));
        push(
            &mut b,
            "Content-Disposition: form-data; name=\"platform\"\r\n\r\n",
        );
        push(&mut b, &format!("{platform}\r\n"));
        push(&mut b, &format!("--{boundary}\r\n"));
        push(
            &mut b,
            "Content-Disposition: form-data; name=\"filename\"\r\n\r\n",
        );
        push(&mut b, &format!("{filename}\r\n"));
        push(&mut b, &format!("--{boundary}\r\n"));
        push(
            &mut b,
            &format!(
                "Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            ),
        );
        b.extend_from_slice(file_bytes);
        push(&mut b, "\r\n");
        push(&mut b, &format!("--{boundary}\r\n"));
        push(
            &mut b,
            "Content-Disposition: form-data; name=\"manifest\"\r\n\r\n",
        );
        push(&mut b, manifest);
        push(&mut b, "\r\n");
        push(&mut b, &format!("--{boundary}--\r\n"));
        (
            axum::http::HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}"))
                .unwrap(),
            b,
        )
    }

    async fn post_json(
        app: axum::Router,
        uri: &str,
        token: Option<&str>,
        body: &str,
    ) -> axum::response::Response {
        let mut rb = Request::builder().method("POST").uri(uri);
        if let Some(t) = token {
            rb = rb.header("Authorization", format!("Bearer {t}"));
        }
        let req = rb
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        app.oneshot(req).await.unwrap()
    }

    async fn upload_one(
        app: &axum::Router,
        token: &str,
        platform: &str,
        filename: &str,
        manifest: &str,
        data: &[u8],
    ) {
        let (ct, body) = multipart_body("BoUnD", platform, filename, manifest, data);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/admin/update")
                    .header("Authorization", format!("Bearer {token}"))
                    .header(CONTENT_TYPE, ct)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "platform={platform}");
    }

    #[tokio::test]
    async fn full_update_flow() {
        let state = test_state(Some("https://sync.example.com"));
        let state = Arc::new(state);
        let app = crate::build_router(state.clone());

        // 1) 未授权上传 → 401
        let (ct, body) = multipart_body(
            "XbOuNdArY",
            "windows-x86_64",
            "test-setup.exe",
            r#"{"version":"0.1.1","platforms":{}}"#,
            b"x",
        );
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/admin/update")
                    .header(CONTENT_TYPE, ct.clone())
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 2) 登录拿 token
        let resp = post_json(
            app.clone(),
            "/api/admin/login",
            None,
            r#"{"user":"admin","pass":"pw"}"#,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let token = v["token"].as_str().unwrap().to_string();

        // 3) 上传 manifest + 文件
        let manifest = r#"{"version":"0.1.1","notes":"t","pub_date":"2026-09-02T00:00:00Z","platforms":{"windows-x86_64":{"url":"test-setup.exe","sha256":"aa"}}}"#;
        let (ct, body) = multipart_body(
            "XbOuNdArY",
            "windows-x86_64",
            "test-setup.exe",
            manifest,
            b"hello-installer",
        );
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/admin/update")
                    .header("Authorization", format!("Bearer {token}"))
                    .header(CONTENT_TYPE, ct)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["version"], json!("0.1.1"));

        // 4) 管理摘要
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/admin/update")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["platforms"]["windows-x86_64"]["uploaded"], json!(true));

        // 5) 公开 latest.json（url 已改写）
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/update/latest.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v["platforms"]["windows-x86_64"]["url"],
            json!("https://sync.example.com/update/files/windows-x86_64/test-setup.exe")
        );

        // 6) 公开下载
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/update/files/windows-x86_64/test-setup.exe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(bytes.as_ref(), b"hello-installer");

        // 7) 目录穿越 / 未知平台 → 404
        for uri in [
            "/update/files/../networks.json",
            "/update/files/etc/x.exe",
            "/update/files/windows-x86_64/%2e%2e%2fsecret",
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "uri={uri}");
        }

        // 8) 未发布前 latest.json → 404（另一个干净 state）
        let state2 = Arc::new(test_state(Some("https://sync.example.com")));
        let app2 = crate::build_router(state2);
        let resp = app2
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/update/latest.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// 各平台独立发布：Windows 先发、darwin 后发，latest.json 必须同时含两条。
    #[tokio::test]
    async fn per_platform_publish_accumulates() {
        let state = Arc::new(test_state(Some("https://sync.example.com")));
        std::fs::create_dir_all(state.update_dir.join("files")).unwrap();
        let app = crate::build_router(state.clone());

        let resp = post_json(
            app.clone(),
            "/api/admin/login",
            None,
            r#"{"user":"admin","pass":"pw"}"#,
        )
        .await;
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let token = v["token"].as_str().unwrap().to_string();

        // Windows 端发布 0.1.1（manifest 里只有自己那一条）
        upload_one(
            &app,
            &token,
            "windows-x86_64",
            "Setup-0.1.1.exe",
            r#"{"version":"0.1.1","notes":"win","pub_date":"2026-09-02T00:00:00Z","platforms":{"windows-x86_64":{"url":"Setup-0.1.1.exe","sha256":"win-sha"}}}"#,
            b"win-bytes",
        )
        .await;

        // darwin 端发布同版本
        upload_one(
            &app,
            &token,
            "darwin-aarch64",
            "ClipSync-0.1.1.dmg",
            r#"{"version":"0.1.1","notes":"mac","pub_date":"2026-09-03T00:00:00Z","platforms":{"darwin-aarch64":{"url":"ClipSync-0.1.1.dmg","sha256":"mac-sha"}}}"#,
            b"mac-bytes",
        )
        .await;

        let raw = std::fs::read_to_string(state.update_dir.join("latest.json")).unwrap();
        let m: UpdateManifest = serde_json::from_str(&raw).unwrap();
        assert_eq!(m.platforms.len(), 2, "两个平台都应留在 manifest 里");
        // sha256 由服务端按**实际落盘内容**自算并覆盖（manifest 里前端提供的 "win-sha"/"mac-sha" 不作数）。
        // 下面两个值分别是 sha256(b"win-bytes") 与 sha256(b"mac-bytes")。
        assert_eq!(
            m.platforms["windows-x86_64"].sha256,
            "178ed8b6329d27f975e0f095ccbb31bc244efbf892dbaff1ba7b5fc0b847caf9"
        );
        assert_eq!(
            m.platforms["darwin-aarch64"].sha256,
            "daf3b7dea44a6ce63b987f3214ae5fb0c3f3c57fdd2a6c50467cea2aaae83001"
        );
        assert_eq!(m.notes, "mac", "描述以最后一次上传为准");

        // 公开读：两个平台的 url 都被改写
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/update/latest.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v["platforms"]["darwin-aarch64"]["url"],
            json!("https://sync.example.com/update/files/darwin-aarch64/ClipSync-0.1.1.dmg")
        );
        assert_eq!(
            v["platforms"]["windows-x86_64"]["url"],
            json!("https://sync.example.com/update/files/windows-x86_64/Setup-0.1.1.exe")
        );
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use axum::extract::State;
    use std::sync::Arc;

    #[test]
    fn version_is_parsed_from_real_asset_names() {
        // 真实发行命名（见 .github/workflows/release.yml）
        for (name, want) in [
            ("ClipSync_0.3.2_x64-setup.exe", "0.3.2"),
            ("ClipSync_0.3.2_aarch64.dmg", "0.3.2"),
            ("ClipSync_0.3.2_amd64.deb", "0.3.2"),
            ("ClipSync_0.3.2_amd64.AppImage", "0.3.2"),
            ("ClipSync-0.3.2-1.x86_64.rpm", "0.3.2"),
            ("ClipSync_1.10.0_arm64.apk", "1.10.0"),
        ] {
            assert_eq!(parse_version_from_name(name).as_deref(), Some(want), "{name}");
        }
        // 平台串里那些数字不能被误当成版本：没有点、点后不是数字、只有两段
        for name in [
            "x86_64.exe",
            "app_amd64.deb",
            "v5.10.pkg",
            "no-version-here.zip",
            "ClipSync_x64-setup.exe",
        ] {
            assert_eq!(parse_version_from_name(name), None, "{name}");
        }
    }

    #[test]
    fn version_sort_key_orders_numerically() {
        // 0.10.0 必须排在 0.9.0 之前（字符串比较会反过来）
        assert!(version_sort_key("0.10.0") > version_sort_key("0.9.0"));
        assert!(version_sort_key("1.0.0") > version_sort_key("0.99.99"));
        // 解析不出来的排最后
        assert!(version_sort_key("未识别版本") < version_sort_key("0.0.1"));
    }

    fn tmp_state() -> Arc<AppState> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("clipsync-hist-ut-{}-{uniq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // update_dir 也要先建出来：有几个用例直接往里写 downloads.json
        std::fs::create_dir_all(dir.join("update")).unwrap();
        Arc::new(AppState {
            store: crate::storage::Store::new(dir.clone()),
            networks: std::sync::Mutex::new(vec![]),
            hub: crate::hub::Hub::new(),
            admin_ws: std::sync::Mutex::new(std::collections::HashMap::new()),
            server_key: "test-key".into(),
            admin_creds: std::sync::Mutex::new(None),
            update_dir: dir.join("update"),
            update_public_base: None,
            update_max_upload: 10 * 1024 * 1024,
            trusted_proxies: vec![],
        })
    }

    #[tokio::test]
    async fn history_groups_by_version_and_marks_current() {
        let state = tmp_state();
        let root = state.update_dir.join("files");
        for p in ["windows-x86_64", "linux-x86_64", "darwin-aarch64"] {
            std::fs::create_dir_all(root.join(p)).unwrap();
        }
        let put = |p: &str, n: &str, size: usize| {
            std::fs::write(root.join(p).join(n), vec![b'x'; size]).unwrap();
        };
        put("windows-x86_64", "ClipSync_0.3.2_x64-setup.exe", 100);
        put("linux-x86_64", "ClipSync_0.3.2_amd64.AppImage", 300);
        put("darwin-aarch64", "ClipSync_0.3.1_aarch64.dmg", 200);
        // 干扰项：上传中断留下的半成品，不该出现在历史里
        put("windows-x86_64", "ClipSync_0.3.2_x64-setup.exe.tmp", 50);
        // 当前线上版本
        std::fs::write(
            state.update_dir.join("latest.json"),
            r#"{"version":"0.3.2","notes":"","pub_date":"","platforms":{}}"#,
        )
        .unwrap();

        let resp = admin_history(State(state.clone())).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(v["current_version"], "0.3.2");
        assert_eq!(v["total_files"], 3, "临时文件不该被算进去");
        let versions = v["versions"].as_array().unwrap();
        assert_eq!(versions.len(), 2, "应分成两个版本组: {v}");
        // 0.3.2 有两个平台、且被标记为当前
        assert_eq!(versions[0]["version"], "0.3.2");
        assert_eq!(versions[0]["is_current"], true);
        assert_eq!(versions[0]["files"].as_array().unwrap().len(), 2);
        assert_eq!(versions[0]["files"][0]["size"], 300); // linux 排在前（按平台名）
        // 0.3.1 只剩 darwin
        assert_eq!(versions[1]["version"], "0.3.1");
        assert_eq!(versions[1]["is_current"], false);
        assert_eq!(versions[1]["files"][0]["platform"], "darwin-aarch64");
    }

    #[tokio::test]
    async fn history_is_empty_and_calm_without_files() {
        let state = tmp_state();
        let resp = admin_history(State(state.clone())).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["total_files"], 0);
        assert_eq!(v["versions"].as_array().unwrap().len(), 0);
        assert!(v["current_version"].is_null());
    }

    // ---------- 公开下载页 ----------

    async fn body_json(resp: Response) -> Value {
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// 造一个「0.3.2 当前 + 0.3.1 历史 + 一个认不出版本的文件」的数据目录
    fn seed_downloads_fixture(state: &Arc<AppState>) {
        let root = state.update_dir.join("files");
        for p in ["windows-x86_64", "darwin-aarch64"] {
            std::fs::create_dir_all(root.join(p)).unwrap();
        }
        std::fs::write(
            root.join("windows-x86_64")
                .join("ClipSync_0.3.2_x64-setup.exe"),
            vec![b'x'; 111],
        )
        .unwrap();
        std::fs::write(
            root.join("darwin-aarch64").join("ClipSync_0.3.1_aarch64.dmg"),
            vec![b'x'; 222],
        )
        .unwrap();
        // 认不出版本的杂物：公开页必须把它藏起来
        std::fs::write(root.join("windows-x86_64").join("leftover.bin"), b"zz").unwrap();
        std::fs::write(
            state.update_dir.join("latest.json"),
            r#"{"version":"0.3.2","notes":"修了几个 bug","pub_date":"2026-09-25T10:00:00Z","platforms":{"windows-x86_64":{"url":"https://x/ClipSync_0.3.2_x64-setup.exe","sha256":"aa"}}}"#,
        )
        .unwrap();
    }

    /// 默认（开关关）：只给当前版本，历史一律不出现在公开数据里。
    #[tokio::test]
    async fn public_versions_hides_history_by_default() {
        let state = tmp_state();
        seed_downloads_fixture(&state);
        let v = body_json(public_versions(State(state.clone())).await).await;

        assert_eq!(v["version"], "0.3.2");
        assert_eq!(v["notes"], "修了几个 bug");
        assert_eq!(v["pub_date"], "2026-09-25T10:00:00Z");
        assert_eq!(v["public_history"], false);
        assert_eq!(v["history"].as_array().unwrap().len(), 0, "默认不该泄露历史");
        assert_eq!(v["platforms"]["windows-x86_64"]["available"], true);
        assert_eq!(v["platforms"]["windows-x86_64"]["size"], 111);
    }

    /// 打开开关：给出历史版本，但仍要藏起「认不出版本」的杂物与当前版本本身。
    #[tokio::test]
    async fn public_versions_lists_history_when_enabled() {
        let state = tmp_state();
        seed_downloads_fixture(&state);
        save_downloads_config(
            &state.update_dir,
            &DownloadsConfig {
                public_history: true,
            },
        )
        .unwrap();

        let v = body_json(public_versions(State(state.clone())).await).await;
        assert_eq!(v["public_history"], true);
        let h = v["history"].as_array().unwrap();
        assert_eq!(h.len(), 1, "只应有一个历史版本: {v}");
        assert_eq!(h[0]["version"], "0.3.1");
        assert_eq!(h[0]["files"][0]["filename"], "ClipSync_0.3.1_aarch64.dmg");
        // 当前版本不重复出现在历史里，杂物分组也不出现
        for g in h {
            assert_ne!(g["version"], "0.3.2");
            assert_ne!(g["version"], UNKNOWN_VERSION);
        }
    }

    /// 安全默认值：配置缺失/坏掉一律当「不公开历史」。
    #[tokio::test]
    async fn downloads_config_fails_closed() {
        let state = tmp_state();
        assert!(!load_downloads_config(&state.update_dir).await.public_history);
        std::fs::write(downloads_config_path(&state.update_dir), b"{ not json").unwrap();
        assert!(!load_downloads_config(&state.update_dir).await.public_history);
        assert!(!serde_json::from_str::<DownloadsConfig>("{}")
            .unwrap()
            .public_history);
    }

    /// 开关端点读写往返。
    #[tokio::test]
    async fn admin_toggle_roundtrip() {
        let state = tmp_state();
        let resp = admin_set_downloads(
            State(state.clone()),
            Json(PublicHistoryBody { enabled: true }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(load_downloads_config(&state.update_dir).await.public_history);

        let resp = admin_set_downloads(
            State(state.clone()),
            Json(PublicHistoryBody { enabled: false }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!load_downloads_config(&state.update_dir).await.public_history);
    }
}
