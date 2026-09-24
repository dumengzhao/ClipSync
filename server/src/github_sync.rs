//! 从 GitHub Release 同步更新包到本服务的更新托管目录（**可选功能**）。
//!
//! ## 为什么需要它
//!
//! GitHub 在部分网络环境下不可达 —— 客户端直连下不动包。但服务端通常能连 GitHub，
//! 于是让服务端把 GitHub 的 latest 发布**转存**到自己这里：
//!
//! ```text
//! https://github.com/{repo}/releases/latest/download/latest.json   ← Release 资产，不走 API 限额
//!   → 逐平台下载安装包（边下边算 sha256，与清单比对）
//!   → 落盘 <update_dir>/files/<platform>/<file>
//!   → merge_manifest 合并进 <update_dir>/latest.json
//! ```
//!
//! 合并后 `/update/latest.json` 自然反映 GitHub 的版本，并且 serve 时
//! [`crate::update::rewrite_urls`] 会把 url 改写成**本机**地址 ——
//! 于是「客户端直连 GitHub 不通」时，走原来的更新入口（relay）照样能更新，
//! **客户端无需任何改动**。
//!
//! ## 开关与触发
//!
//! - `GITHUB_REPO`（如 `owner/name`）未配置 = **功能整体关闭**；
//! - 定时同步默认**关闭**，可在管理页开启（配置持久化到 `<data_dir>/github-sync.json`）；
//! - 手动触发：管理页「从 GitHub 同步」按钮（`POST /api/admin/github-sync/run`）。
//!
//! ## 安全边界（都是必须的）
//!
//! - **只接受 https + 白名单域名**（`github.com` / `objects.githubusercontent.com`）：
//!   清单来自 GitHub，但不能让它把我们指向任意主机（SSRF）；
//! - 只同步 `releases/latest`（**草稿与预发布不计入** ✓ 与人工发布流程天然一致）；
//! - 文件名过 `is_safe_filename`、平台键过 `is_valid_platform`（与公开端点同一套校验）；
//! - **sha256 必须匹配**才落盘；同名文件哈希不同 → **拒绝并告警**（不静默覆盖，
//!   避免出现「同一版本两个不同二进制」）；
//! - 版本**只升不降**：GitHub 版本低于当前线上版本时直接跳过。
//!
//! ## 传输为什么走系统 `curl` 而不是 reqwest
//!
//! 服务端的交付形态是 **musl 静态单文件**（`install.sh` 整目录拷过去即用）。而
//! `reqwest` + `rustls` 会拉进 `ring`（含 C 代码），ring 编译需要 **musl C 编译器**
//! —— 本机与部署机都没有、且没有 sudo ✗，一旦引入就再也出不了静态产物 ✗。
//! 因此这里用系统 `curl`（Linux 发行版标配、`install.sh` 本身也依赖它）取数据，
//! 既拿到成熟的 TLS 校验，又让 crate 依赖树保持**纯 Rust** ✓。
//! 用 `cargo tree -p clipsync-server | grep -E "ring|aws-lc"` 应无输出，可随时自证。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::state::AppState;
use crate::update::{
    basename_of, is_safe_filename, is_valid_platform, merge_manifest, validate_manifest,
    UpdateManifest,
};

/// 允许发起下载的域名：GitHub 本体 + 资产实际落点（Release 资产会 302 到后者）。
const ALLOWED_HOSTS: &[&str] = &["github.com", "objects.githubusercontent.com"];
/// GitHub 侧清单资产名（由 CI 的 release 工作流生成并上传）。
const MANIFEST_ASSET: &str = "latest.json";
/// 同步配置文件名（放在 data_dir 下，与 server.key 等同级）。
const CONFIG_FILE: &str = "github-sync.json";
/// 清单文本上限（正常只有几 KB）。
const MANIFEST_MAX_BYTES: usize = 1024 * 1024;
/// 单个安装包上限：2 GiB（安装包不可能更大，防异常清单让我们写爆磁盘）。
const MAX_ASSET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// 默认定时间隔（分钟）：12 小时。
const DEFAULT_INTERVAL_MINUTES: u64 = 720;
/// 定时检查的轮询间隔：每分钟看一次「到点了吗」。
const TICK: Duration = Duration::from_secs(60);

// ---------- 配置 ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    /// 定时同步开关（默认关闭）
    #[serde(default)]
    pub enabled: bool,
    /// 定时间隔（分钟）
    #[serde(default = "default_interval")]
    pub interval_minutes: u64,
    /// 上次运行时间（RFC3339，仅用于展示）
    #[serde(default)]
    pub last_run_at: Option<String>,
    /// 上次运行结果摘要（仅用于展示）
    #[serde(default)]
    pub last_result: Option<String>,
}

fn default_interval() -> u64 {
    DEFAULT_INTERVAL_MINUTES
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: DEFAULT_INTERVAL_MINUTES,
            last_run_at: None,
            last_result: None,
        }
    }
}

/// `GITHUB_REPO`（`owner/name`）；未配置或为空 = 功能关闭。
pub fn repo() -> Option<String> {
    std::env::var("GITHUB_REPO")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CONFIG_FILE)
}

pub fn load_config(data_dir: &Path) -> SyncConfig {
    std::fs::read_to_string(config_path(data_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_config(data_dir: &Path, cfg: &SyncConfig) -> Result<(), String> {
    let text = serde_json::to_string_pretty(cfg).map_err(|e| format!("序列化配置失败: {e}"))?;
    crate::storage::atomic_write(&config_path(data_dir), &text).map_err(|e| format!("写入配置失败: {e}"))
}

// ---------- 纯函数（可单测） ----------

/// 语义化版本粗比较：逐段比较「主.次.补丁」的数值。
///
/// 返回 `true` 表示 `candidate` **不低于** `current`（即允许同步）。
/// 任一侧解析失败时退化为「字符串相等才算不低」—— 宁可少同步，也不要把线上版本降级。
pub fn not_older(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> {
        let core = v.trim().trim_start_matches('v');
        let core = core.split(['-', '+']).next().unwrap_or(core);
        core.split('.').map(|p| p.parse::<u64>().ok()).collect()
    };
    match (parse(candidate), parse(current)) {
        (Some(a), Some(b)) => a >= b,
        _ => candidate.trim() == current.trim(),
    }
}

/// 下载地址是否可接受：必须 https，且 host **精确**等于白名单之一。
///
/// 手写解析而不用 `url` crate：这里只需要「https + 主机名精确匹配」，而任何宽松解析都可能
/// 被 `https://evil@github.com/…`（userinfo）或 `https://github.com.evil.com/…`（后缀伪装）绕过 ✗。
/// 做法：只认 `https://` 前缀 → 取到第一个 `/`、`?`、`#` 为止的 authority → **出现 `:` 或 `@` 一律拒绝**
/// （我们自己的清单 URL 从不带端口与 userinfo）→ 最后要求与白名单完全相等。
pub fn url_allowed(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains(':') || authority.contains('@') {
        return false;
    }
    ALLOWED_HOSTS.contains(&authority)
}

/// curl 公共参数：静默、跟随重定向（GitHub 资产会 302 到 objects.githubusercontent.com）、
/// 且**只允许 https**（含重定向）—— 与 `url_allowed` 构成双重防线。
///
/// 刻意不加 `--fail`：取清单时要拿到 HTTP 状态码自行判断（404 = 还没有已发布的 latest）。
fn curl_common() -> [&'static str; 7] {
    [
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
    ]
}

/// 取一段小文本（清单）：返回 (HTTP 状态码, 正文)。
async fn curl_text(url: &str, max_bytes: usize) -> Result<(u16, String), String> {
    let out = tokio::process::Command::new("curl")
        .args(curl_common())
        .args(["--max-time", "60", "--write-out", "\n%{http_code}", "-o", "-"])
        .arg(url)
        .output()
        .await
        .map_err(|e| format!("无法启动 curl（服务端需要系统安装 curl）: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "curl 调用失败（退出码 {:?}）: {}",
            out.status.code(),
            err.trim()
        ));
    }
    if out.stdout.len() > max_bytes + 64 {
        return Err(format!("响应超过 {max_bytes} 字节上限"));
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, code) = match text.rsplit_once('\n') {
        Some((b, c)) => (b.to_string(), c.trim().parse::<u16>().unwrap_or(0)),
        None => (text, 0),
    };
    Ok((code, body))
}

// ---------- 同步结果 ----------

#[derive(Debug, Clone, Serialize)]
pub struct PlatformOutcome {
    pub platform: String,
    pub file: String,
    /// `downloaded`（新下载）/ `skipped`（本地已有且哈希一致）/ `rejected`（原因在 detail）
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SyncReport {
    pub repo: String,
    pub version: String,
    pub outcomes: Vec<PlatformOutcome>,
    /// 合并进本地清单的平台数
    pub merged_platforms: usize,
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// 下载 `url` 到 `dst` 并校验 sha256。
///
/// - 本地已存在且哈希与清单一致 → 直接跳过（不重复下载）；
/// - 本地已存在但哈希不同 → **拒绝**（同版本不同二进制必须人工介入，不能静默覆盖）；
/// - 先写 `.part` 再 rename：中途失败不会留下半截文件被当成完成品。
async fn download_verified(
    platform: &str,
    url: &str,
    expected: &str,
    dst: &Path,
) -> Result<&'static str, String> {
    let expected = expected.trim().to_lowercase();
    if dst.exists() {
        match sha256_file(dst) {
            Ok(got) if got == expected => return Ok("skipped"),
            Ok(got) => {
                return Err(format!(
                    "本地已有同名文件但哈希不同（本地 {got}，清单 {expected}）——已拒绝覆盖，请人工确认"
                ))
            }
            Err(e) => return Err(format!("读取本地已有文件失败: {e}")),
        }
    }
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("创建目录失败: {e}"))?;
    }
    let tmp = dst.with_extension("part");
    let _ = tokio::fs::remove_file(&tmp).await;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // `--fail`：非 2xx 直接非零退出，不会把错误页当安装包写下去
    let mut child = tokio::process::Command::new("curl")
        .args(curl_common())
        .args(["--fail", "--max-time", "600", "-o", "-"])
        .arg(url)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法启动 curl（服务端需要系统安装 curl）: {e}"))?;
    let mut stdout = match child.stdout.take() {
        Some(s) => s,
        None => return Err("无法读取 curl 输出".to_string()),
    };
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| format!("创建临时文件失败: {e}"))?;
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match stdout.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = child.kill().await;
                drop(file);
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(format!("读取下载流失败: {e}"));
            }
        };
        total += n as u64;
        if total > MAX_ASSET_BYTES {
            // 超限要**先杀掉 curl**，否则它会继续下载把磁盘写满
            let _ = child.kill().await;
            drop(file);
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(format!(
                "超出单包上限 {} GiB",
                MAX_ASSET_BYTES / (1024 * 1024 * 1024)
            ));
        }
        hasher.update(&buf[..n]);
        if let Err(e) = file.write_all(&buf[..n]).await {
            let _ = child.kill().await;
            drop(file);
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(format!("写入失败: {e}"));
        }
    }
    if let Err(e) = file.flush().await {
        let _ = child.kill().await;
        drop(file);
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(format!("落盘失败: {e}"));
    }
    drop(file);
    // 读完流再看退出码：curl 失败时绝不能把半截文件当成下载完成
    let status = child
        .wait()
        .await
        .map_err(|e| format!("等待 curl 结束失败: {e}"))?;
    if !status.success() {
        let mut err = String::new();
        if let Some(mut se) = child.stderr.take() {
            let _ = se.read_to_string(&mut err).await;
        }
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(format!(
            "下载失败（curl 退出码 {:?}）: {}",
            status.code(),
            err.trim()
        ));
    }
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != expected {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(format!("sha256 校验失败（清单 {expected}，实际 {got}）——已丢弃下载文件"));
    }
    tokio::fs::rename(&tmp, dst)
        .await
        .map_err(|e| format!("重命名失败: {e}"))?;
    println!("[clipsync-server] GitHub 同步：已下载 {platform}/{}（{total} 字节）", basename_of(url));
    Ok("downloaded")
}

fn files_root(state: &AppState) -> PathBuf {
    state.update_dir.join("files")
}

fn now_rfc3339() -> String {
    // 只用于展示，精度要求不高：用系统时间转 ISO8601（UTC）
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 手写格式化，避免为一个展示字段引入 chrono
    let (days, secs) = (now / 86_400, now % 86_400);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    // 1970-01-01 起的天数 → 年月日（Civil from days 算法）
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mth <= 2 { y + 1 } else { y };
    format!("{year:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

// ---------- 同步主流程 ----------

/// 执行一次同步。返回报告；任何**致命**问题（未配置 / 清单取不到 / 全部平台失败）都返回 Err。
pub async fn sync_once(state: &Arc<AppState>) -> Result<SyncReport, String> {
    let repo = repo().ok_or_else(|| "未配置 GITHUB_REPO，GitHub 同步功能未启用".to_string())?;

    let manifest_url =
        format!("https://github.com/{repo}/releases/latest/download/{MANIFEST_ASSET}");
    let (code, raw) = curl_text(&manifest_url, MANIFEST_MAX_BYTES).await?;
    if code == 404 {
        return Err(
            "GitHub 上没有已发布的 latest release，或该 release 未附 latest.json（草稿与预发布不计入）"
                .to_string(),
        );
    }
    if !(200..300).contains(&code) {
        return Err(format!("GitHub 返回 HTTP {code}"));
    }
    let incoming = validate_manifest(&raw)?;

    let latest_path = state.update_dir.join("latest.json");
    let existing: Option<UpdateManifest> = tokio::fs::read_to_string(&latest_path)
        .await
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    if let Some(cur) = &existing {
        if !not_older(&incoming.version, &cur.version) {
            return Err(format!(
                "GitHub 版本 {} 低于当前线上版本 {}，已跳过（拒绝降级）",
                incoming.version, cur.version
            ));
        }
    }

    let version = incoming.version.clone();
    let mut outcomes: Vec<PlatformOutcome> = Vec::new();
    let mut merged = UpdateManifest {
        version: incoming.version.clone(),
        notes: incoming.notes.clone(),
        pub_date: incoming.pub_date.clone(),
        platforms: Default::default(),
    };

    for (platform, entry) in &incoming.platforms {
        let name = basename_of(&entry.url);
        let reject = |outcomes: &mut Vec<PlatformOutcome>, detail: String| {
            outcomes.push(PlatformOutcome {
                platform: platform.clone(),
                file: name.clone(),
                status: "rejected".to_string(),
                detail: Some(detail),
            });
        };
        if !is_valid_platform(platform) || !is_safe_filename(&name) {
            reject(&mut outcomes, "平台键或文件名不合法".to_string());
            continue;
        }
        if !url_allowed(&entry.url) {
            reject(&mut outcomes, format!("下载地址不在允许的域名内: {}", entry.url));
            continue;
        }
        let dst = files_root(state).join(platform).join(&name);
        match download_verified(platform, &entry.url, &entry.sha256, &dst).await {
            Ok(status) => {
                outcomes.push(PlatformOutcome {
                    platform: platform.clone(),
                    file: name.clone(),
                    status: status.to_string(),
                    detail: None,
                });
                // 只有真的拿到字节（含"本地已有且一致"）才把它写进清单
                merged
                    .platforms
                    .insert(platform.clone(), entry.clone());
            }
            Err(e) => {
                eprintln!("[clipsync-server] GitHub 同步：平台 {platform} 失败：{e}");
                reject(&mut outcomes, e);
            }
        }
    }

    if merged.platforms.is_empty() {
        return Err(format!(
            "没有任何平台同步成功（{}），已放弃写入清单",
            outcomes
                .iter()
                .map(|o| format!("{}: {}", o.platform, o.detail.clone().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join("；")
        ));
    }

    let merged_count = merged.platforms.len();
    let final_manifest = merge_manifest(existing, merged);
    let text = serde_json::to_string_pretty(&final_manifest)
        .map_err(|e| format!("序列化清单失败: {e}"))?;
    let tmp = state.update_dir.join("latest.json.tmp");
    tokio::fs::write(&tmp, text)
        .await
        .map_err(|e| format!("写清单失败: {e}"))?;
    tokio::fs::rename(&tmp, &latest_path)
        .await
        .map_err(|e| format!("替换清单失败: {e}"))?;

    println!(
        "[clipsync-server] GitHub 同步完成：{repo} 版本 {version}，{} 个平台入库（本次合并 {merged_count} 个）",
        outcomes.len()
    );
    Ok(SyncReport {
        repo,
        version,
        outcomes,
        merged_platforms: merged_count,
    })
}

/// 定时任务入口：每分钟被调用一次，只有「开启且到点」才真正同步。
pub async fn tick(state: &Arc<AppState>) {
    if repo().is_none() {
        return; // 未配置 = 功能关闭，连配置都不用读
    }
    let cfg = load_config(&state.store.dir);
    if !cfg.enabled {
        return;
    }
    let due = match cfg.last_run_at.as_deref().and_then(parse_rfc3339_epoch) {
        Some(last) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            now.saturating_sub(last) >= cfg.interval_minutes.saturating_mul(60)
        }
        None => true,
    };
    if !due {
        return;
    }
    // 成败都已在 run_and_record 内部记录并打印，这里显式忽略返回值
    let _ = run_and_record(state).await;
}

/// 跑一次并把结果写进配置（last_run_at / last_result），供管理页展示。
pub async fn run_and_record(state: &Arc<AppState>) -> Result<SyncReport, String> {
    let result = sync_once(state).await;
    let mut cfg = load_config(&state.store.dir);
    cfg.last_run_at = Some(now_rfc3339());
    cfg.last_result = Some(match &result {
        Ok(r) => format!(
            "OK：{} 版本 {}，{} 个平台（合并 {}）",
            r.repo,
            r.version,
            r.outcomes.len(),
            r.merged_platforms
        ),
        Err(e) => format!("失败：{e}"),
    });
    if let Err(e) = save_config(&state.store.dir, &cfg) {
        eprintln!("[clipsync-server] GitHub 同步：记录结果失败：{e}");
    }
    if let Err(e) = &result {
        eprintln!("[clipsync-server] GitHub 同步失败：{e}");
    }
    result
}

/// 极简 RFC3339（`YYYY-MM-DDTHH:MM:SSZ`）→ epoch 秒；解析失败返回 None（视为「从未运行」）。
fn parse_rfc3339_epoch(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.len() != 20 || !s.ends_with('Z') {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    // days from civil（与上面格式化相反）
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec;
    u64::try_from(secs).ok()
}

// ---------- 管理端点（注册在 admin_auth 之下） ----------

/// POST /api/admin/github-sync/run —— 立即同步一次。
pub async fn admin_run(State(state): State<Arc<AppState>>) -> Response {
    match run_and_record(&state).await {
        Ok(report) => (StatusCode::OK, Json(serde_json::json!({ "ok": true, "report": report })))
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": e })),
        )
            .into_response(),
    }
}

/// GET /api/admin/github-sync —— 当前配置与上次结果（管理页展示用）。
pub async fn admin_get(State(state): State<Arc<AppState>>) -> Response {
    let cfg = load_config(&state.store.dir);
    Json(serde_json::json!({
        "configured": repo().is_some(),
        "repo": repo(),
        "enabled": cfg.enabled,
        "interval_minutes": cfg.interval_minutes,
        "last_run_at": cfg.last_run_at,
        "last_result": cfg.last_result,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct ConfigBody {
    pub enabled: bool,
    pub interval_minutes: u64,
}

/// POST /api/admin/github-sync/config —— 开关与间隔。
pub async fn admin_set_config(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ConfigBody>,
) -> Response {
    if body.interval_minutes < 5 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "间隔至少 5 分钟" })),
        )
            .into_response();
    }
    let mut cfg = load_config(&state.store.dir);
    cfg.enabled = body.enabled;
    cfg.interval_minutes = body.interval_minutes;
    match save_config(&state.store.dir, &cfg) {
        Ok(()) => {
            println!(
                "[clipsync-server] GitHub 同步配置已更新：enabled={} interval={}min",
                cfg.enabled,
                cfg.interval_minutes
            );
            Json(serde_json::json!({ "ok": true, "enabled": cfg.enabled, "interval_minutes": cfg.interval_minutes }))
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}

/// 后台轮询任务：每分钟检查一次是否到点（未配置/未开启时几乎零开销）。
pub fn spawn_ticker(state: Arc<AppState>) {
    if repo().is_none() {
        println!("[clipsync-server] GitHub 同步未启用（未配置 GITHUB_REPO）");
        return;
    }
    tokio::spawn(async move {
        // 启动后先等一个 tick，避免与启动流程抢 IO
        loop {
            tokio::time::sleep(TICK).await;
            tick(&state).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_guard_blocks_downgrade() {
        assert!(not_older("0.4.0", "0.3.2"));
        assert!(not_older("0.3.2", "0.3.2"));
        assert!(not_older("v0.3.2", "0.3.2"));
        assert!(!not_older("0.3.1", "0.3.2"));
        assert!(!not_older("0.2.9", "0.3.0"));
        // 解析失败时退化为字符串比较：不相等即视为「更旧」→ 拒绝，宁可少同步
        assert!(!not_older("not-a-version", "0.3.2"));
    }

    #[test]
    fn url_allowlist_only_github_https() {
        assert!(url_allowed(
            "https://github.com/o/r/releases/download/v1/ClipSync_1_x64-setup.exe"
        ));
        assert!(url_allowed(
            "https://objects.githubusercontent.com/github-production-release-asset/x"
        ));
        assert!(!url_allowed("http://github.com/o/r/x")); // 明文
        assert!(!url_allowed("https://evil.example.com/x")); // 非白名单
        assert!(!url_allowed("https://github.com.evil.example.com/x")); // 后缀伪装
        assert!(!url_allowed("file:///etc/passwd"));
        assert!(!url_allowed("https://evil@github.com/x")); // userinfo 伪装
        assert!(!url_allowed("https://github.com:8443/x")); // 我们的清单不带端口，出现即拒
    }

    #[test]
    fn rfc3339_roundtrip() {
        let t = now_rfc3339();
        let secs = parse_rfc3339_epoch(&t);
        assert!(secs.is_some());
        assert!(secs.unwrap() > 1_700_000_000, "时间戳应在 2023 年之后: {t}");
    }

    #[test]
    fn config_roundtrip_with_unknown_fields() {
        // 旧配置缺字段时必须能用默认值补齐（前向兼容）
        let cfg: SyncConfig = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.interval_minutes, DEFAULT_INTERVAL_MINUTES);
    }
}
