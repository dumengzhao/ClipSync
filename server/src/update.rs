//! 客户端更新托管模块（无签名自托管模型，见 server/UPDATE_MODULE_PLAN.md）。
//!
//! - 公开读：`GET /update/latest.json`（url 按本机 origin/UPDATE_PUBLIC_BASE 改写）、
//!   `GET /update/files/:platform/:file`（整文件流式返回）。
//! - 管理：`GET /api/admin/update` 当前版本摘要、`POST /api/admin/update` multipart 上传
//!   （字段顺序约定：每组文件前先发 `platform`、`filename` 文本字段，再发 `file`）。
//! - 信任模型：无签名，完整性校验靠 manifest 的 sha256；来源真伪由 TLS + 服务器保证。
//! - 落盘沿用 storage.rs 套路：写 `*.tmp` 再 rename 原子替换，避免半截文件被拉走。

use crate::state::AppState;
use axum::body::Body;
use axum::extract::{Multipart, Path, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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
pub fn effective_base(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<String, String> {
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
    if let Some(origin) = headers.get(axum::http::header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if origin.starts_with("https://") {
            return Ok(origin.trim_end_matches('/').to_string());
        }
    }
    Err("cannot determine https base for update urls: set UPDATE_PUBLIC_BASE (e.g. https://sync.example.com)".to_string())
}

/// 把 manifest 中各平台 url 改写为 `<base>/update/files/<platform>/<basename>`。
pub fn rewrite_urls(manifest: &mut Value, base: &str) {
    let Some(platforms) = manifest.get_mut("platforms").and_then(|p| p.as_object_mut()) else {
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

/// 校验 manifest 结构：version 非空、platforms 非空、键在白名单内、每项含 url+sha256。
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
        if e.sha256.trim().is_empty() {
            return Err(format!("platform {p}: sha256 required"));
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
    let raw = match std::fs::read_to_string(&path) {
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
    (
        [(CONTENT_TYPE, "application/json")],
        v.to_string(),
    )
        .into_response()
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

// ---------- 管理端点（admin_auth 之下） ----------

/// GET /api/admin/update —— 当前线上版本摘要（无发布则 404）。
pub async fn admin_info(State(state): State<Arc<AppState>>) -> Response {
    let path = state.update_dir.join("latest.json");
    let raw = match std::fs::read_to_string(&path) {
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
        let (uploaded, size) = match std::fs::metadata(&fp) {
            Ok(md) => (true, md.len()),
            Err(_) => (false, 0),
        };
        platforms.insert(
            p.clone(),
            json!({
                "filename": name,
                "sha256": e.sha256,
                "uploaded": uploaded,
                "size": if uploaded { Value::from(size) } else { Value::Null },
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
    let mut uploaded: Vec<(String, String, u64)> = Vec::new();
    let mut total: u64 = 0;

    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                return err(StatusCode::BAD_REQUEST, format!("multipart error: {e}")).await
            }
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
                            return err(
                                StatusCode::BAD_REQUEST,
                                format!("manifest field: {e}"),
                            )
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
                    return err(StatusCode::BAD_REQUEST, format!("unknown platform: {platform}"))
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
                let mut f = field;
                loop {
                    match f.chunk().await {
                        Ok(Some(c)) => {
                            size += c.len() as u64;
                            total += c.len() as u64;
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
                            return err(
                                StatusCode::BAD_REQUEST,
                                format!("file stream: {e}"),
                            )
                            .await;
                        }
                    }
                }
                if let Err(e) = out.sync_all().await {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("sync file: {e}"),
                    )
                    .await;
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
                uploaded.push((platform, filename, size));
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
    let existing: Option<UpdateManifest> = match tokio::fs::read_to_string(&latest_dst).await {
        Ok(s) => match serde_json::from_str::<UpdateManifest>(&s) {
            Ok(m) => Some(m),
            Err(_) => None, // 现有文件坏了：以本次上传为准重建，不让一次解析失败卡死发布
        },
        Err(_) => None,
    };
    let previous_version = existing.as_ref().map(|e| e.version.clone());
    let merged_from_existing = existing.is_some();
    let m = merge_manifest(existing, incoming);

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
            "uploaded": uploaded.iter().map(|(p, f, s)| json!({
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
        let ok = r#"{"version":"0.1.1","platforms":{"windows-x86_64":{"url":"a.exe","sha256":"aa"}}}"#;
        assert!(validate_manifest(ok).is_ok());
        let bad_platform = r#"{"version":"0.1.1","platforms":{"etc/passwd":{"url":"a","sha256":"aa"}}}"#;
        assert!(validate_manifest(bad_platform).is_err());
        let no_sha = r#"{"version":"0.1.1","platforms":{"windows-x86_64":{"url":"a.exe","sha256":""}}}"#;
        assert!(validate_manifest(no_sha).is_err());
        let no_version = r#"{"version":"","platforms":{"windows-x86_64":{"url":"a.exe","sha256":"aa"}}}"#;
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
            out.platforms["darwin-aarch64"].url,
            "ClipSync-0.1.0.dmg",
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
        h.insert(axum::http::header::HOST, "sync.example.com".parse().unwrap());
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
        assert_eq!(effective_base(&state2, &HeaderMap::new()).unwrap(), "https://cdn.example.com");
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
            admin_user: "admin".into(),
            admin_pass_hash: crate::storage::hash_pass("pw"),
            update_dir: dir.join("update"),
            update_public_base: public_base.map(|s| s.to_string()),
            update_max_upload: 10 * 1024 * 1024,
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
        push(&mut b, "Content-Disposition: form-data; name=\"platform\"\r\n\r\n");
        push(&mut b, &format!("{platform}\r\n"));
        push(&mut b, &format!("--{boundary}\r\n"));
        push(&mut b, "Content-Disposition: form-data; name=\"filename\"\r\n\r\n");
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
        push(&mut b, "Content-Disposition: form-data; name=\"manifest\"\r\n\r\n");
        push(&mut b, manifest);
        push(&mut b, "\r\n");
        push(&mut b, &format!("--{boundary}--\r\n"));
        (
            axum::http::HeaderValue::from_str(&format!(
                "multipart/form-data; boundary={boundary}"
            ))
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
        assert_eq!(m.platforms["windows-x86_64"].sha256, "win-sha");
        assert_eq!(m.platforms["darwin-aarch64"].sha256, "mac-sha");
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
