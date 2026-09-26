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
//! ## 代理（服务器直连不通 GitHub 时用）
//!
//! 生效优先级从高到低：
//! 1. 管理页「GitHub 同步 → 出网代理」（持久化到 `github-sync.json`）→ curl 加 `--proxy`；
//! 2. 进程环境变量 `HTTPS_PROXY` / `https_proxy` / `ALL_PROXY`（在 systemd 的
//!    `EnvironmentFile` 里配一行即可，curl 自己会读，**不需要改代码**）；
//! 3. 都没配 = 直连。
//!
//! 支持 `http://`、`https://`、`socks5://`、`socks5h://` —— 最后一个是**域名交给代理解析**，
//! 服务器自身 DNS 解不出 github.com 时靠它。代理串里的口令不会进日志也不会回传前端
//! （见 [`redact_proxy`] 与 `PROXY_UNCHANGED` 哨兵）。
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
/// 默认定时：每天凌晨 3:00（**本地时区**）。
///
/// 用 cron 而不是「每 N 分钟」，是因为真实需求都是「每天几点」「每周几几点」这类
/// 日历语义 —— 固定间隔表达不了，而且间隔会从上次运行时刻漂移。
pub const DEFAULT_CRON: &str = "0 3 * * *";
/// 定时检查的轮询间隔：每分钟看一次「到点了吗」。cron 的精度是分钟，够了。
const TICK: Duration = Duration::from_secs(60);
/// 两次触发之间允许的最小间隔：低于这个值直接拒绝保存。
///
/// 同步要走 GitHub API 并可能下载几十 MB 的安装包，太频繁既打搅对方也浪费带宽。
const MIN_CRON_GAP: Duration = Duration::from_secs(5 * 60);

// ---------- 配置 ----------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncConfig {
    /// 定时同步开关（默认关闭）
    #[serde(default)]
    pub enabled: bool,
    /// 定时表达式（**标准 5 段 cron**：`分 时 日 月 周`，按**服务器本地时区**）。
    ///
    /// 空 = 用 `DEFAULT_CRON`。解析与校验见 `validate_cron`——注意 cron crate 内部
    /// 要 6 段（含秒），`normalize_cron` 会自动补一个前导 `0`。
    #[serde(default)]
    pub cron: Option<String>,
    /// 下次触发时刻（RFC3339 UTC，`Z` 结尾）—— 顺带供管理页展示。
    #[serde(default)]
    pub next_run_at: Option<String>,
    /// ⚠️ 旧字段（分钟间隔），**只用于把老配置迁移成 cron**，新代码不要再读它。
    #[serde(default)]
    pub interval_minutes: Option<u64>,
    /// 上次运行时间（RFC3339，仅用于展示）
    #[serde(default)]
    pub last_run_at: Option<String>,
    /// 上次运行结果摘要（仅用于展示）
    #[serde(default)]
    pub last_result: Option<String>,
    /// 出网代理（如 `http://127.0.0.1:7890`、`socks5h://user:pass@host:1080`）。
    ///
    /// `None` = **不显式指定**，此时系统 `curl` 仍会自动读环境变量
    /// `HTTPS_PROXY` / `https_proxy` / `ALL_PROXY`（见模块文档的「代理」一节）。
    #[serde(default)]
    pub proxy: Option<String>,
    /// 仓库 `owner/name`（管理页可改）。为空则回退环境变量 `GITHUB_REPO`。
    #[serde(default)]
    pub repo: Option<String>,
}

// ---------- cron ----------

/// 把用户写法规范化成 cron crate 需要的 6 段（补秒）。
///
/// 页面上填的是标准 5 段（`分 时 日 月 周`），cron crate 要的是
/// `秒 分 时 日 月 周`；拿到 5 段就在前面补一个 `0`（整分触发）。
fn normalize_cron(raw: &str) -> Result<String, String> {
    let v = raw.trim();
    if v.is_empty() {
        return Err("定时表达式不能为空".to_string());
    }
    let n = v.split_whitespace().count();
    match n {
        5 => Ok(format!("0 {v}")),
        6 => Ok(v.to_string()),
        _ => Err(format!(
            "定时表达式需 5 段（分 时 日 月 周），现在是 {n} 段；示例：0 3 * * *"
        )),
    }
}

/// 解析 cron 表达式。失败信息直接给管理员看，所以带上示例。
fn parse_cron(raw: &str) -> Result<cron::Schedule, String> {
    let expr = normalize_cron(raw)?;
    expr.parse::<cron::Schedule>()
        .map_err(|e| format!("定时表达式无法解析：{e}（示例：0 3 * * * = 每天 3:00）"))
}

/// 校验 cron 表达式：能解析，且**两次触发的间隔不小于 `MIN_CRON_GAP`**。
///
/// 间隔下限只能这样算：cron 语法本身没有「周期」这个概念，`*/1 * * * *` 和
/// `0,2,4 * * * *` 都能写出很密的时间表。取最近两次触发求差最可靠。
/// 返回规范化后的表达式（6 段）。
pub fn validate_cron(raw: &str) -> Result<String, String> {
    let schedule = parse_cron(raw)?;
    // 看**多**个触发点求最短间隔，不能只比前两次：像 `0,2,4 * * *`（每小时的 0/2/4 分）
    // 前两次可能恰好跨了小时边界（20:04 → 21:00），看着很稀疏，实际是每 2 分钟一次。
    let min_gap = chrono::Duration::seconds(MIN_CRON_GAP.as_secs() as i64);
    let mut prev: Option<chrono::DateTime<chrono::Local>> = None;
    let mut smallest: Option<chrono::Duration> = None;
    let mut hits = 0usize;
    for t in schedule.upcoming(chrono::Local).take(12) {
        if let Some(p) = prev {
            let gap = t.signed_duration_since(p);
            smallest = Some(match smallest {
                Some(m) if m <= gap => m,
                _ => gap,
            });
        }
        prev = Some(t);
        hits += 1;
    }
    if hits < 2 {
        return Err("定时表达式永远不会被触发".to_string());
    }
    if let Some(g) = smallest {
        if g < min_gap {
            return Err(format!(
                "触发过于频繁：最短间隔约 {} 秒，至少需要 {} 秒",
                g.num_seconds(),
                MIN_CRON_GAP.as_secs()
            ));
        }
    }
    // 前面已经解析过一次，这里必然成功
    normalize_cron(raw)
}

/// 算出「从现在起的下一个触发时刻」，写成 RFC3339 UTC（`Z` 结尾，
/// 与 `last_run_at` 同格式，`parse_rfc3339_epoch` 才认）。
fn next_run_after_now(schedule: &cron::Schedule) -> Option<String> {
    schedule.upcoming(chrono::Local).next().map(|t| {
        t.with_timezone(&chrono::Utc)
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    })
}

/// 旧配置迁移：`interval_minutes`（每 N 分钟）→ cron。
///
/// 只在读到老配置文件（没有 `cron` 字段）时走一次；转化不了精度就往上取整到小时，
/// 宁可稀一点也不写出不合法的表达式。
fn migrate_interval_to_cron(minutes: u64) -> String {
    if minutes < 60 {
        format!("*/{} * * * *", minutes.max(1))
    } else {
        let h = (minutes / 60).clamp(1, 23);
        format!("0 */{h} * * *")
    }
}

/// 取生效的 cron 表达式（配置为空则用默认），并顺手把「老配置 → cron」的迁移做掉。
fn effective_cron(cfg: &SyncConfig) -> String {
    match cfg.cron.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(c) => c.to_string(),
        None => cfg
            .interval_minutes
            .map(migrate_interval_to_cron)
            .unwrap_or_else(|| DEFAULT_CRON.to_string()),
    }
}

/// `GITHUB_REPO`（`owner/name`）；未配置或为空 = 功能关闭。**环境变量只是来源之一**。
pub fn repo() -> Option<String> {
    std::env::var("GITHUB_REPO")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 仓库名校验：只接受 `owner/name`（两段，各由字母数字与 `-_.` 组成）。
///
/// 这个值会被拼进 `https://github.com/{repo}/releases/...`，宽松校验等于给 SSRF 开后门
/// （`../`、`@`、`://` 之类一律不许出现）。
pub fn validate_repo(raw: &str) -> Result<String, String> {
    let v = raw.trim();
    if v.is_empty() {
        return Err("仓库名为空".to_string());
    }
    if v.len() > 200 {
        return Err("仓库名过长（上限 200 字符）".to_string());
    }
    let mut parts = v.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err("仓库名格式应为 owner/name（如 dumengzhao/ClipSync）".to_string());
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !ok(owner) || !ok(name) {
        return Err("仓库名只能含字母、数字、减号、下划线和点，且形如 owner/name".to_string());
    }
    Ok(v.to_string())
}

/// 实际生效的仓库：**管理页配置优先**，没有才回退环境变量。
///
/// 页面里改了就必须立刻生效（否则用户会以为改了没用）；环境变量保留给
/// 「首次部署时写在 systemd env 文件里」的老流程 —— 两种来源都能用。
pub fn repo_for(state: &AppState) -> Option<String> {
    load_config(&state.store.dir)
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(repo)
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
    crate::storage::atomic_write(&config_path(data_dir), &text)
        .map_err(|e| format!("写入配置失败: {e}"))
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

/// 允许的代理 scheme（`curl --proxy` 认识的取值）。
///
/// 刻意**不含** `socks4`（无 DNS 代理能力、已淘汰）；也不接受裸 `host:port`
/// （curl 会按 http 处理，但读起来极易与 socks5 混淆，逼用户写清楚）。
/// 其中 `socks5h` = **域名交给代理解析**（远端 DNS），本机解析不了 GitHub 时靠它。
const PROXY_SCHEMES: &[&str] = &["http", "https", "socks5", "socks5h"];
/// 前端「代理输入框没改动」时回传的哨兵值（与 `network_token` 的哨兵同一套思路）：
/// 因为接口返回给前端的是**脱敏串**（口令是 `***`），原样回传会把真口令改成三个星号。
pub const PROXY_UNCHANGED: &str = "__clipsync_proxy_unchanged__";
/// 代理串长度上限（防把一大段乱七八糟的东西塞进配置文件）。
const PROXY_MAX_LEN: usize = 300;
/// 账号 / 口令各自的长度上限。
const PROXY_CRED_MAX_LEN: usize = 200;

/// 代理串是否可接受；返回值是**规范化后**的串（已 trim），错误原因直接给管理页展示。
///
/// 要点：① scheme 在白名单内；② 有 host 和端口；③ 无空白/控制字符 ——
/// 既防参数注入（虽然 `Command` 不经 shell），也防伪造换行污染日志。
pub fn validate_proxy(raw: &str) -> Result<String, String> {
    let v = raw.trim();
    if v.is_empty() {
        return Err("代理地址为空".to_string());
    }
    if v.len() > PROXY_MAX_LEN {
        return Err(format!("代理地址过长（上限 {PROXY_MAX_LEN} 字符）"));
    }
    if v.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("代理地址不能含空白或控制字符".to_string());
    }
    let (scheme, rest) = v.split_once("://").ok_or_else(|| {
        "代理地址必须带协议前缀，如 http://1.2.3.4:7890 或 socks5h://1.2.3.4:1080".to_string()
    })?;
    if !PROXY_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str()) {
        return Err(format!(
            "不支持的代理协议 `{scheme}`，可选：{}",
            PROXY_SCHEMES.join(" / ")
        ));
    }
    // authority = [userinfo@]host[:port]；先截到第一个 `/`，再从右边剥掉 userinfo
    let authority = rest.split('/').next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    // userinfo 里再出现 `@` 必须拒绝：curl 是按**第一个** `@` 切分 userinfo 与 host 的
    // （实测 `http://u:p@ss@host:port` 会直接报
    //  `Unsupported proxy syntax ... Port number was not a decimal number`，退出码 5，
    //  连代理都没连上）。必须让用户写成 `%40`，否则「保存成功、同步时才炸」最难排查。
    if let Some((userinfo, _)) = authority.rsplit_once('@') {
        if userinfo.contains('@') {
            return Err("口令里的 @ 必须写成 %40（curl 按第一个 @ 切分，明文 @ 会报 Unsupported proxy syntax）".to_string());
        }
    }
    // 分开看主机名与端口：`http://:7890` 这种「只有端口」也要挡掉
    let name = host.split(':').next().unwrap_or("");
    let port = host.rsplit(':').next().unwrap_or("");
    if name.is_empty() {
        return Err("代理地址缺少主机名".to_string());
    }
    if !host.contains(':') || port.is_empty() {
        return Err("代理地址缺少端口（如 http://1.2.3.4:7890）".to_string());
    }
    Ok(v.to_string())
}

/// 用「地址 + **已编码**的 userinfo」拼出完整代理串。
///
/// 管理页把地址/账号/口令分成三个输入框（口令框 `type=password`），**不让用户自己拼 URL**：
/// userinfo 必须 percent 编码（尤其 `@`），否则 curl 按第一个 `@` 切分会直接报
/// `Unsupported proxy syntax`（退出码 5，连代理都连不上）。
///
/// `userinfo` 为 `None` 时表示不需要认证，直接返回地址本身。
pub fn compose_proxy(url: &str, userinfo: Option<&str>) -> Result<String, String> {
    if url.trim().is_empty() {
        return Err("代理地址为空".to_string());
    }
    if url.contains('@') {
        return Err("代理地址不要带账号口令，请填在旁边的「代理账号 / 代理口令」里".to_string());
    }
    let v = validate_proxy(url)?;
    let Some(ui) = userinfo.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(v);
    };
    let (scheme, rest) = v.split_once("://").unwrap_or(("", v.as_str()));
    let full = format!("{scheme}://{ui}@{rest}");
    if full.len() > PROXY_MAX_LEN {
        return Err(format!("代理串过长（上限 {PROXY_MAX_LEN} 字符）"));
    }
    Ok(full)
}

/// 把明文账号 / 口令拼成**已编码**的 userinfo（口令为空时只带账号）。
pub fn build_userinfo(user: &str, pass: &str) -> Option<String> {
    let u = user.trim();
    if u.is_empty() {
        return None;
    }
    if u.len() > PROXY_CRED_MAX_LEN || pass.len() > PROXY_CRED_MAX_LEN {
        return None;
    }
    if u.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    if pass.is_empty() {
        return Some(pct_encode(u));
    }
    Some(format!("{}:{}", pct_encode(u), pct_encode(pass)))
}

/// percent-encoding：只保留 RFC3986 的 unreserved 字符，其余一律 `%XX`。
///
/// 口令里出现 `@` `:` `/` `#` 时若不编码，curl 会把代理串切错（实测退出码 5）；
/// 编码后 curl 会正确解码成原始口令。
pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 把完整代理串拆成给管理页用的三份：**口令永远不返回**。
/// 返回 `(地址, 账号, 是否有口令)`。
pub fn split_proxy(raw: &str) -> (String, String, bool) {
    let Some((scheme, rest)) = raw.split_once("://") else {
        return (raw.to_string(), String::new(), false);
    };
    match rest.rsplit_once('@') {
        Some((userinfo, host)) => {
            let (user, pass) = match userinfo.split_once(':') {
                Some((u, p)) => (u, Some(p)),
                None => (userinfo, None),
            };
            (
                format!("{scheme}://{host}"),
                pct_decode(user),
                pass.is_some(),
            )
        }
        None => (raw.to_string(), String::new(), false),
    }
}

/// 取已保存代理串里的 userinfo **原始编码形式**。
///
/// 「口令没改动」时必须原样复用，否则会把已经 `%40` 过的口令再编码一遍（变成 `%2540`）。
fn encoded_userinfo(raw: &str) -> Option<String> {
    let (_, rest) = raw.split_once("://")?;
    let (userinfo, _) = rest.rsplit_once('@')?;
    Some(userinfo.to_string())
}

/// `pct_encode` 的逆运算（只用于把已保存的用户名显示回输入框，不必覆盖全部边界）。
fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hexval(bytes[i + 1]), hexval(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hexval(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 日志与接口返回用的脱敏：`scheme://user:pass@host` → `scheme://user:***@host`。
/// 用户名保留（便于辨认是哪个账号），**口令一律打码**。
pub fn redact_proxy(raw: &str) -> String {
    let Some((scheme, rest)) = raw.split_once("://") else {
        return raw.to_string();
    };
    let Some((userinfo, host)) = rest.rsplit_once('@') else {
        return raw.to_string();
    };
    let user = userinfo.split_once(':').map(|(u, _)| u).unwrap_or(userinfo);
    format!("{scheme}://{user}:***@{host}")
}

/// 把错误文本里可能出现的**代理串**替换成脱敏形式。
///
/// ⚠️ 这条防线不是多虑：curl 在代理相关错误里会**原样回显整个代理 URL**，例如
/// `curl: (5) Unsupported proxy syntax in 'http://alice:s3cr3t@1.2.3.4:7890'` ——
/// 而这段文本会被我们包进错误信息，再**记进日志、返回给管理页**。
/// 不处理的话，我们费力做的「口令不回显」就绕过去了（口令明文躺在日志文件里）。
fn scrub_proxy_in(text: &str, proxy: Option<&str>) -> String {
    match proxy.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) if text.contains(p) => text.replace(p, &redact_proxy(p)),
        _ => text.to_string(),
    }
}

/// curl 公共参数：静默、跟随重定向（GitHub 资产会 302 到 objects.githubusercontent.com）、
/// 且**只允许 https**（含重定向）—— 与 `url_allowed` 构成双重防线。
///
/// 刻意不加 `--fail`：取清单时要拿到 HTTP 状态码自行判断（404 = 还没有已发布的 latest）。
///
/// `proxy` 非空时加 `--proxy <串>`（curl 能识别其中的 socks5/socks5h scheme）；
/// 为空则**不传**，交给 curl 自己读环境变量（见模块文档「代理」一节）。
fn curl_args(proxy: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = [
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    if let Some(p) = proxy {
        args.push("--proxy".to_string());
        args.push(p.to_string());
    }
    args
}

/// 进程环境里是否已经设了代理（curl 会自动读这些变量，不用我们传参）。
fn env_proxy() -> Option<String> {
    for k in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        if let Ok(v) = std::env::var(k) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// 下载失败时的补充提示：**没配任何代理**时，八成是服务器直连不通 GitHub，
/// 明确告诉运维下一步该做什么，比只丢一句「curl 退出码 35」有用得多。
fn hint_if_no_proxy(err: String, proxy: Option<&str>) -> String {
    if proxy.is_some() || env_proxy().is_some() {
        return err;
    }
    format!(
        "{err}（服务器可能直连不通 GitHub：可在管理页「GitHub 同步」配置代理，\
         或给服务进程设置 HTTPS_PROXY 环境变量后重启）"
    )
}

/// 取一段小文本（清单）：返回 (HTTP 状态码, 正文)。
async fn curl_text(
    url: &str,
    max_bytes: usize,
    proxy: Option<&str>,
    timeout_secs: u64,
) -> Result<(u16, String), String> {
    let out = tokio::process::Command::new("curl")
        .args(curl_args(proxy))
        .args([
            "--max-time",
            &timeout_secs.to_string(),
            "--write-out",
            "\n%{http_code}",
            "-o",
            "-",
        ])
        .arg(url)
        .output()
        .await
        .map_err(|e| {
            hint_if_no_proxy(
                format!("无法启动 curl（服务端需要系统安装 curl）: {e}"),
                proxy,
            )
        })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(hint_if_no_proxy(
            format!(
                "curl 调用失败（退出码 {:?}）: {}",
                out.status.code(),
                // curl 的错误里可能带着完整代理串（含口令）—— 必须脱敏后才能外传
                scrub_proxy_in(err.trim(), proxy)
            ),
            proxy,
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
    proxy: Option<&str>,
) -> Result<&'static str, String> {
    let expected = expected.trim().to_lowercase();
    if dst.exists() {
        match sha256_file(dst) {
            Ok(got) if got == expected => return Ok("skipped"),
            Ok(got) => {
                // 消息里带两个哈希，写成独立变量让 rustfmt 有稳定的布局
                // （直接内联时它会在这两种排版之间来回震荡，`cargo fmt --check` 永远不通过）
                let msg = format!(
                    "本地已有同名文件但哈希不同（本地 {got}，清单 {expected}）——已拒绝覆盖，请人工确认"
                );
                return Err(msg);
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
        .args(curl_args(proxy))
        .args(["--fail", "--max-time", "600", "-o", "-"])
        .arg(url)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            hint_if_no_proxy(
                format!("无法启动 curl（服务端需要系统安装 curl）: {e}"),
                proxy,
            )
        })?;
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
        return Err(hint_if_no_proxy(
            format!(
                "下载失败（curl 退出码 {:?}）: {}",
                status.code(),
                // 同 curl_text：错误里可能回显含口令的代理串，先脱敏
                scrub_proxy_in(err.trim(), proxy)
            ),
            proxy,
        ));
    }
    let got: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if got != expected {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(format!(
            "sha256 校验失败（清单 {expected}，实际 {got}）——已丢弃下载文件"
        ));
    }
    tokio::fs::rename(&tmp, dst)
        .await
        .map_err(|e| format!("重命名失败: {e}"))?;
    tracing::info!("已下载 {platform}/{}（{total} 字节）", basename_of(url));
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
    let repo = repo_for(state).ok_or_else(|| {
        "未配置仓库：请在管理页填写 owner/name，或给服务进程设置 GITHUB_REPO".to_string()
    })?;
    let cfg = load_config(&state.store.dir);
    let proxy = cfg.proxy.as_deref();
    if let Some(p) = proxy {
        tracing::info!("本次走代理 {}", redact_proxy(p));
    }

    let manifest_url =
        format!("https://github.com/{repo}/releases/latest/download/{MANIFEST_ASSET}");
    let (code, raw) = curl_text(&manifest_url, MANIFEST_MAX_BYTES, proxy, 60).await?;
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
            reject(
                &mut outcomes,
                format!("下载地址不在允许的域名内: {}", entry.url),
            );
            continue;
        }
        let dst = files_root(state).join(platform).join(&name);
        match download_verified(platform, &entry.url, &entry.sha256, &dst, proxy).await {
            Ok(status) => {
                outcomes.push(PlatformOutcome {
                    platform: platform.clone(),
                    file: name.clone(),
                    status: status.to_string(),
                    detail: None,
                });
                // 只有真的拿到字节（含"本地已有且一致"）才把它写进清单
                merged.platforms.insert(platform.clone(), entry.clone());
            }
            Err(e) => {
                tracing::warn!("平台 {platform} 失败：{e}");
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

    tracing::info!(
        "同步完成：{} 版本 {}，{} 个平台入库（本次合并 {merged_count} 个）",
        crate::logging::clean(&repo),
        // 版本号来自 GitHub 的清单：也过一遍 clean，免得异常字符把日志搅乱
        crate::logging::clean(&version),
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
    if repo_for(state).is_none() {
        return; // 未配置 = 功能关闭，连配置都不用读
    }
    let cfg = load_config(&state.store.dir);
    if !cfg.enabled {
        return;
    }
    // cron 到点了吗？`next_run_at` 存在配置里：没到点就什么都不做，
    // 到点了（或还从没算过）才真正跑一次，跑完立刻算出下一次的时间。
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cron = effective_cron(&cfg);
    let (run, next_write) = plan_tick(&cron, cfg.next_run_at.as_deref(), now);
    if let Some(next) = next_write {
        let mut cfg2 = cfg.clone();
        cfg2.next_run_at = Some(next.clone());
        if let Err(e) = save_config(&state.store.dir, &cfg2) {
            tracing::warn!("记录下次运行时间失败：{e}");
        } else if run {
            tracing::info!("定时触发同步（cron={cron:?}）→ 下次运行 {next}");
        }
    }
    if !run {
        // 未到点时留一条 debug 痕：排查「为什么没自动同步」先看这里
        // （表达式无效的情况 plan_tick 已经 warn 过，不再重复）
        if let Some(next) = cfg.next_run_at.as_deref().and_then(parse_rfc3339_epoch) {
            if now < next {
                tracing::debug!("定时未到点（下次 {}），本轮跳过", utc_stamp(next));
            }
        }
        return;
    }
    // 成败都已在 run_and_record 内部记录并打印，这里显式忽略返回值
    let _ = run_and_record(state).await;
}

/// 表达式坏掉（被手工改坏等）时把下一次往后推多久再试。
const CRON_ERROR_BACKOFF: u64 = 3600;

/// epoch 秒 → RFC3339 UTC（`Z` 结尾，与 `parse_rfc3339_epoch` 对称）。
fn utc_stamp(epoch: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(epoch as i64, 0)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

/// 决定这一轮 tick 该做什么：返回 `(是否执行同步, 要写回的 next_run_at)`。
///
/// 抽成纯函数是为了能测 —— 这里藏着一个不容易发现的坑：
/// **表达式解析失败时不能「什么都不写就继续跑」**。因为 `next_run_at` 已经过期，
/// 不更新的话下一分钟 `due` 依然成立 → 变成**每分钟同步一次**，把 GitHub 和带宽打爆。
/// 所以解析失败要显式退避（推后 `CRON_ERROR_BACKOFF` 再试），并且**本轮不跑**。
fn plan_tick(cron: &str, next_run_at: Option<&str>, now: u64) -> (bool, Option<String>) {
    let due = match next_run_at.and_then(parse_rfc3339_epoch) {
        Some(next) => now >= next,
        None => true, // 没算过（首次开启 / 旧配置）→ 立刻补算一次时间点
    };
    if !due {
        return (false, None); // 没到点：配置不动
    }
    match parse_cron(cron) {
        // 先算出下一次，再跑：即使同步耗时很久，也不会把同一个 cron 点跑两遍
        Ok(schedule) => (true, next_run_after_now(&schedule)),
        Err(e) => {
            tracing::warn!(
                "定时表达式无效（{e}），本轮跳过，{CRON_ERROR_BACKOFF} 秒后再试\
                 （改好表达式后保存即可立即恢复）"
            );
            (false, Some(utc_stamp(now + CRON_ERROR_BACKOFF)))
        }
    }
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
        tracing::warn!("记录同步结果失败：{e}");
    }
    if let Err(e) = &result {
        tracing::error!("同步失败：{e}");
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
        Ok(report) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "report": report })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": e })),
        )
            .into_response(),
    }
}

/// GET /api/admin/github-sync —— 当前配置与上次结果（管理页展示用）。
///
/// `proxy` 返回的是**脱敏串**（口令为 `***`），原样回传会被当成真值 ——
/// 所以同时下发哨兵 `proxy_unchanged_sentinel`，前端「没改动」时回传它（见 `PROXY_UNCHANGED`）。
pub async fn admin_get(State(state): State<Arc<AppState>>) -> Response {
    let cfg = load_config(&state.store.dir);
    // 给管理页三个独立输入框用：**口令永远不下发**，只说「有没有保存过」
    let (proxy_url, proxy_user, has_password) = cfg
        .proxy
        .as_deref()
        .map(split_proxy)
        .unwrap_or_else(|| (String::new(), String::new(), false));
    Json(serde_json::json!({
        "configured": repo_for(&state).is_some(),
        "repo": repo_for(&state),
        // 当前值来自哪里（页面 / 环境变量），以及环境变量本身的值 —— 管理页要如实展示
        "repo_source": if load_config(&state.store.dir).repo.as_deref().map(str::trim).filter(|s| !s.is_empty()).is_some() { "config" } else { "env" },
        "env_repo": repo(),
        "enabled": cfg.enabled,
        // 展示用 5 段写法（用户填什么就显示什么），`next_run_at` 供页面提示下次运行
        "cron": effective_cron(&cfg),
        "next_run_at": cfg.next_run_at,
        "last_run_at": cfg.last_run_at,
        "last_result": cfg.last_result,
        "proxy_set": cfg.proxy.is_some(),
        "proxy_url": proxy_url,
        "proxy_user": proxy_user,
        "proxy_has_password": has_password,
        // 下面两个仅用于状态展示与兼容老客户端
        "proxy": cfg.proxy.as_deref().map(redact_proxy),
        "proxy_unchanged_sentinel": PROXY_UNCHANGED,
        // 进程环境里已设代理（curl 会自动读）：提示用，避免用户以为「没配就没代理」
        "env_proxy": env_proxy().is_some(),
    }))
    .into_response()
}

/// POST /api/admin/github-sync/test —— 只探连通性（不落盘、不改清单）。
///
/// 用**真实同步时那个 URL** 去试（不是 api.github.com），这样结果才代表同步能不能成：
/// 只要能拿到 HTTP 状态码，就说明 DNS + TCP + TLS + 代理隧道全通。
pub async fn admin_test(State(state): State<Arc<AppState>>) -> Response {
    let Some(repo) = repo_for(&state) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": "未配置 GITHUB_REPO" })),
        )
            .into_response();
    };
    let cfg = load_config(&state.store.dir);
    let proxy = cfg.proxy.as_deref();
    let url = format!("https://github.com/{repo}/releases/latest/download/{MANIFEST_ASSET}");
    let started = std::time::Instant::now();
    match curl_text(&url, MANIFEST_MAX_BYTES, proxy, 30).await {
        Ok((code, body)) => {
            let ms = started.elapsed().as_millis();
            match code {
                // 200：拿到清单了，顺手解析版本给用户看
                200..300 => {
                    let version = validate_manifest(&body)
                        .map(|m| m.version)
                        .unwrap_or_else(|e| format!("（清单解析失败：{e}）"));
                    Json(serde_json::json!({
                        "ok": true, "http_code": code, "ms": ms, "version": version,
                        "proxy": proxy.map(redact_proxy),
                        "detail": "能取到 GitHub 上的 latest.json，同步链路通畅",
                    }))
                    .into_response()
                }
                // 404 = 网络是通的（浏览器能直连的话这里就是 302，服务端 curl 会跟到 200）
                404 => Json(serde_json::json!({
                    "ok": true, "http_code": code, "ms": ms,
                    "proxy": proxy.map(redact_proxy),
                    "detail": "能连上 GitHub，但该仓库还没有已发布的 latest release（草稿与预发布不计入）",
                }))
                .into_response(),
                other => Json(serde_json::json!({
                    "ok": false, "http_code": other, "ms": ms,
                    "proxy": proxy.map(redact_proxy),
                    "error": format!("GitHub 返回 HTTP {other}"),
                }))
                .into_response(),
            }
        }
        Err(e) => Json(serde_json::json!({
            "ok": false,
            "proxy": proxy.map(redact_proxy),
            "ms": started.elapsed().as_millis(),
            "error": e,
        }))
        .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct ConfigBody {
    pub enabled: bool,
    /// cron 表达式（5 段）；`None` = 不改动
    #[serde(default)]
    pub cron: Option<String>,
    /// ⚠️ 旧字段（分钟间隔）：只有老客户端会发，收到就迁移成 cron
    #[serde(default)]
    pub interval_minutes: Option<u64>,
    /// 旧字段：完整代理串（含 userinfo）。管理页现在改用下面三个字段。
    #[serde(default)]
    pub proxy: Option<String>,
    /// 代理地址，形如 `socks5h://1.2.3.4:1080`（**不带账号口令**）
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// 代理账号
    #[serde(default)]
    pub proxy_user: Option<String>,
    /// 代理口令；`Some("")` = 清除口令，`Some(PROXY_UNCHANGED)` / `None` = 不改动
    #[serde(default)]
    pub proxy_pass: Option<String>,
    /// 仓库 `owner/name`；`None` = 不改动，`Some("")` = 清空（回退到环境变量 `GITHUB_REPO`）
    #[serde(default)]
    pub repo: Option<String>,
}

/// POST /api/admin/github-sync/config —— 开关、间隔、代理。
pub async fn admin_set_config(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ConfigBody>,
) -> Response {
    let mut cfg = load_config(&state.store.dir);
    cfg.enabled = body.enabled;
    // 定时表达式：给了就校验后写入；没给但发了旧的 interval_minutes → 迁移成 cron
    let cron_input = match body.cron.as_deref() {
        Some(c) => Some(c.to_string()),
        None => body.interval_minutes.map(migrate_interval_to_cron),
    };
    if let Some(raw) = cron_input {
        match validate_cron(&raw) {
            Ok(_normalized) => {
                cfg.cron = Some(raw.trim().to_string());
                // 表达式变了 → 下次触发时间必须重算，否则会照着旧时间表跑
                cfg.next_run_at = None;
            }
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": e })),
                )
                    .into_response();
            }
        }
    }
    // 仓库：缺字段 = 不改动；空串 = 清空（回退环境变量）；其余校验后写入
    match body.repo.as_deref() {
        None => {}
        Some(r) => {
            let t = r.trim();
            if t.is_empty() {
                cfg.repo = None;
            } else {
                match validate_repo(t) {
                    Ok(v) => cfg.repo = Some(v),
                    Err(e) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({ "error": e })),
                        )
                            .into_response();
                    }
                }
            }
        }
    }
    let mut proxy_note = "不变".to_string();
    // 结构化三件套（地址 / 账号 / 口令）优先；只用旧字段 `proxy` 也能工作（向后兼容）
    let structured =
        body.proxy_url.is_some() || body.proxy_user.is_some() || body.proxy_pass.is_some();
    if structured {
        let (cur_url, cur_user, _has_pw) = cfg
            .proxy
            .as_deref()
            .map(split_proxy)
            .unwrap_or_else(|| (String::new(), String::new(), false));
        // 已保存的 userinfo（**编码形式**，原样复用，避免二次编码）
        let cur_userinfo = cfg.proxy.as_deref().and_then(encoded_userinfo);

        let url = body.proxy_url.clone().unwrap_or(cur_url); // 缺字段 = 不变
        let user = body.proxy_user.clone().unwrap_or_else(|| cur_user.clone());
        let user_changed = user != cur_user;

        let userinfo: Option<String> = match body.proxy_pass.as_deref() {
            // 口令没动：账号也没动就整段沿用；账号换了则只能带新账号（无口令）
            Some(p) if p == PROXY_UNCHANGED => {
                if user_changed {
                    build_userinfo(&user, "")
                } else {
                    cur_userinfo
                }
            }
            None => cur_userinfo,
            // 显式清空口令
            Some("") => build_userinfo(&user, ""),
            Some(p) => build_userinfo(&user, p),
        };

        if url.trim().is_empty() {
            cfg.proxy = None;
            proxy_note = "已清空".to_string();
        } else {
            match compose_proxy(&url, userinfo.as_deref()) {
                Ok(v) => {
                    proxy_note = redact_proxy(&v);
                    cfg.proxy = Some(v);
                }
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": e })),
                    )
                        .into_response();
                }
            }
        }
    } else {
        match body.proxy.as_deref() {
            None => {}
            Some(p) if p == PROXY_UNCHANGED => {}
            Some(p) => {
                let t = p.trim();
                if t.is_empty() {
                    cfg.proxy = None;
                    proxy_note = "已清空".to_string();
                } else {
                    match validate_proxy(t) {
                        Ok(v) => {
                            proxy_note = redact_proxy(&v);
                            cfg.proxy = Some(v);
                        }
                        Err(e) => {
                            return (
                                StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({ "error": e })),
                            )
                                .into_response();
                        }
                    }
                }
            }
        }
    }
    // 时间点跟着配置走：改了表达式就重算（否则会照着旧时间表跑），
    // 关掉定时就清空（页面上不该显示一个不会发生的「下次运行」）。
    if !cfg.enabled {
        cfg.next_run_at = None;
    } else if cfg.next_run_at.is_none() {
        // 这里直接算出来，不等每分钟的 tick —— 管理员保存完就该看到「下次运行」时间
        if let Ok(schedule) = parse_cron(&effective_cron(&cfg)) {
            cfg.next_run_at = next_run_after_now(&schedule);
        }
    }
    match save_config(&state.store.dir, &cfg) {
        Ok(()) => {
            tracing::info!(
                "配置已更新：enabled={} cron={:?} next={:?} proxy={}",
                cfg.enabled,
                effective_cron(&cfg),
                cfg.next_run_at,
                proxy_note
            );
            let (pu, pusr, has_pw) = cfg
                .proxy
                .as_deref()
                .map(split_proxy)
                .unwrap_or_else(|| (String::new(), String::new(), false));
            Json(serde_json::json!({
                "ok": true,
                "enabled": cfg.enabled,
                "cron": effective_cron(&cfg),
                "next_run_at": cfg.next_run_at,
                "repo": repo_for(&state),
                "repo_source": if cfg.repo.is_some() { "config" } else { "env" },
                "env_repo": repo(),
                "proxy_set": cfg.proxy.is_some(),
                "proxy_url": pu,
                "proxy_user": pusr,
                "proxy_has_password": has_pw,
                "proxy": cfg.proxy.as_deref().map(redact_proxy),
            }))
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
///
/// 启动时会把「有没有配仓库、定时开没开、cron 是什么、下次什么时候跑」全部打出来 ——
/// 「为什么没自动同步」这类问题基本靠这三行日志就能定位。
pub fn spawn_ticker(state: Arc<AppState>) {
    let Some(repo) = repo_for(&state) else {
        tracing::info!("GitHub 同步未启用（未配置仓库：管理页填 owner/name 或设置 GITHUB_REPO）");
        return;
    };
    let cfg = load_config(&state.store.dir);
    let cron = effective_cron(&cfg);
    if cfg.enabled {
        tracing::info!(
            "GitHub 同步定时已启用：仓库={repo} cron={cron:?}（按服务器本地时区）\
             下次运行={}",
            cfg.next_run_at
                .as_deref()
                .unwrap_or("未计算（启动后一分钟内补算）")
        );
    } else {
        tracing::info!(
            "GitHub 同步定时未开启：仓库={repo} 已配置，cron={cron:?} —— \
             可在管理页开启定时，或手动点「立即同步」"
        );
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
        // 旧配置没有 cron 字段 ⇒ None，取默认值（effective_cron 负责兜底）
        assert!(cfg.cron.is_none());
        assert_eq!(effective_cron(&cfg), DEFAULT_CRON);
        assert!(cfg.proxy.is_none(), "旧配置没有 proxy 字段 ⇒ 必须是 None");
    }

    #[test]
    fn proxy_validation_accepts_known_schemes() {
        assert_eq!(
            validate_proxy("http://1.2.3.4:7890").unwrap(),
            "http://1.2.3.4:7890"
        );
        assert_eq!(
            validate_proxy("  socks5h://user:pw@1.2.3.4:1080  ").unwrap(), // 首尾空白应被 trim
            "socks5h://user:pw@1.2.3.4:1080"
        );
        assert!(validate_proxy("https://proxy.example.com:3128").is_ok());
        assert!(validate_proxy("socks5://127.0.0.1:1080").is_ok());
        // 口令里的 @ 必须 URL 编码（curl 按第一个 @ 切分，明文 @ 会让它解析错端口）
        assert!(validate_proxy("http://alice:p%40ss%3Aword@1.2.3.4:7890").is_ok());
        // 口令里的「:」本身没问题（curl 取第一个 : 之后全部作为口令）
        assert!(validate_proxy("http://alice:p:ss@1.2.3.4:7890").is_ok());
    }

    #[test]
    fn proxy_validation_rejects_raw_at_in_password() {
        // 这条是实测踩出来的：curl 会因此报 "Unsupported proxy syntax"（退出码 5）
        let e = validate_proxy("http://alice:p@ss:word@1.2.3.4:7890").unwrap_err();
        assert!(e.contains("%40"), "错误提示必须告诉用户改用 %40，实际: {e}");
    }

    #[test]
    fn proxy_validation_rejects_bad_input() {
        for bad in [
            "",                               // 空
            "1.2.3.4:7890",                   // 无 scheme
            "ftp://1.2.3.4:21",               // 非白名单协议
            "socks4://1.2.3.4:1080",          // 已淘汰
            "http://1.2.3.4",                 // 缺端口
            "http://:7890",                   // 缺主机
            "http://1.2.3.4:",                // 空端口
            "http://1.2.3.4:",                // 空端口
            "http://1.2.3.4:7890\nX-Evil: 1", // 换行注入（污染日志/参数）
            "http://1.2.3.4:7890 x",          // 含空格
        ] {
            assert!(validate_proxy(bad).is_err(), "应拒绝: {bad:?}");
        }
        assert!(validate_proxy(&"http://1.2.3.4:7890/".repeat(40)).is_err()); // 超长
    }

    // ---------- cron ----------

    /// tick 的决策：这里钉住「表达式坏掉时不能退化成每分钟同步」。
    #[test]
    fn tick_plan_backs_off_instead_of_hammering() {
        // 必须用**真实当前时间**做基准：`next_run_after_now` 内部取的是 `Local::now()`，
        // 用假时间戳（未来/过去）会让两边对不上，测出来的只是幻觉。
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // 1) 表达式坏了 + next_run_at 已过期 → **不跑**，且必须写回一个未来的退避时间，
        //    否则下一分钟 due 仍然成立，会变成每分钟同步一次。
        let (run, next) = plan_tick("not a cron", Some(&utc_stamp(now - 10)), now);
        assert!(!run, "表达式无效时不能执行同步");
        let next = next.expect("必须写入退避时间，否则会高频重试");
        let next_epoch = parse_rfc3339_epoch(&next).expect("退避时间要是合法 UTC 串");
        assert!(next_epoch > now, "退避时间必须在未来: {next}");
        assert_eq!(next_epoch, now + CRON_ERROR_BACKOFF);

        // 2) 未到点 → 不跑、不动配置
        let (run, next) = plan_tick("0 3 * * *", Some(&utc_stamp(now + 100)), now);
        assert!(!run);
        assert!(next.is_none(), "没到点不该改写 next_run_at");

        // 3) 正常到点 → 跑，并写入未来的下次时间
        let (run, next) = plan_tick("0 3 * * *", Some(&utc_stamp(now - 1)), now);
        assert!(run);
        let next_epoch = parse_rfc3339_epoch(&next.unwrap()).unwrap();
        assert!(next_epoch > now);

        // 4) 从没算过（None）→ 也算到点，跑一次
        let (run, next) = plan_tick("*/30 * * * *", None, now);
        assert!(run);
        assert!(next.is_some());
    }

    #[test]
    fn utc_stamp_roundtrips_with_parser() {
        // tick 写回的时间必须能被自己的解析器读回来，否则调度会失准
        for t in [0u64, 1_800_000_000, 2_000_000_000] {
            let s = utc_stamp(t);
            assert_eq!(s.len(), 20, "{s}");
            assert!(s.ends_with('Z'));
            assert_eq!(parse_rfc3339_epoch(&s), Some(t));
        }
    }

    #[test]
    fn cron_accepts_standard_five_fields() {
        // 页面上填的是标准 5 段，内部补成 6 段（含秒）
        assert_eq!(normalize_cron("0 3 * * *").unwrap(), "0 0 3 * * *");
        assert_eq!(normalize_cron("  30 4 * * 1  ").unwrap(), "0 30 4 * * 1");
        // 直接给 6 段也认
        assert_eq!(normalize_cron("0 0 3 * * *").unwrap(), "0 0 3 * * *");
        // 段数不对 / 空 都要报「需 5 段」而不是默默接受
        assert!(normalize_cron("").is_err());
        assert!(normalize_cron("0 3 * *").is_err());
        assert!(normalize_cron("0 0 3 * * * *").is_err());
    }

    #[test]
    fn cron_validation_rejects_garbage_and_too_frequent() {
        for bad in [
            "not a cron",    // 完全不是
            "* * *",         // 段数不对
            "99 3 * * *",    // 分钟越界
            "0 25 * * *",    // 小时越界
            "*/1 * * * *",   // 每 1 分钟：太频繁
            "0,2,4 * * * *", // 每小时 3 次：间隔 2 分钟，同样太频繁
        ] {
            assert!(validate_cron(bad).is_err(), "应拒绝: {bad:?}");
        }
        // 合理的一律通过
        for ok in ["0 3 * * *", "*/30 * * * *", "0 */6 * * *", "0 3 * * 1"] {
            assert!(validate_cron(ok).is_ok(), "应接受: {ok:?}");
        }
    }

    #[test]
    fn cron_next_run_is_in_the_future_and_utc() {
        let s = parse_cron("0 3 * * *").unwrap();
        let next = next_run_after_now(&s).expect("应能算出下次时间");
        // 必须是 `parse_rfc3339_epoch` 认得的格式（20 字符、Z 结尾）
        assert_eq!(next.len(), 20, "next_run_at 格式: {next}");
        assert!(next.ends_with('Z'));
        assert!(parse_rfc3339_epoch(&next).is_some(), "要能被解析回 epoch");

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let next_epoch = parse_rfc3339_epoch(&next).unwrap();
        assert!(next_epoch > now, "下次触发必须在未来：{next}");
        // 每天 3:00 → 距现在不超过 24 小时
        assert!(next_epoch - now <= 24 * 3600);
    }

    #[test]
    fn legacy_interval_minutes_migrates_to_cron() {
        assert_eq!(migrate_interval_to_cron(30), "*/30 * * * *");
        assert_eq!(migrate_interval_to_cron(5), "*/5 * * * *");
        assert_eq!(migrate_interval_to_cron(720), "0 */12 * * *");
        assert_eq!(migrate_interval_to_cron(60), "0 */1 * * *");
        // 边界：0 会写出非法表达式 → 至少 1 分钟；超大值钳到 23 小时
        assert_eq!(migrate_interval_to_cron(0), "*/1 * * * *");
        assert_eq!(migrate_interval_to_cron(100_000), "0 */23 * * *");
        // 迁移结果本身必须能通过校验（除了小于 5 分钟的那种老配置，那就让用户重填）
        assert!(validate_cron(&migrate_interval_to_cron(720)).is_ok());
        assert!(validate_cron(&migrate_interval_to_cron(30)).is_ok());
    }

    #[test]
    fn effective_cron_falls_back_to_default_then_migration() {
        let mut cfg = SyncConfig::default();
        assert_eq!(effective_cron(&cfg), DEFAULT_CRON);
        // 只有旧字段 → 迁移
        cfg.interval_minutes = Some(120);
        assert_eq!(effective_cron(&cfg), "0 */2 * * *");
        // 有了 cron 就以它为准
        cfg.cron = Some("15 4 * * *".into());
        assert_eq!(effective_cron(&cfg), "15 4 * * *");
        // 空串也当没填
        cfg.cron = Some("   ".into());
        assert_eq!(effective_cron(&cfg), "0 */2 * * *");
    }

    /// curl 的报错里会**原样回显整个代理串（含口令）**，而这段文本要进日志与管理页 ——
    /// 所以必须在包装前脱敏。这条测试就是钉住「日志里绝不出现代理口令」。
    #[test]
    fn curl_error_text_never_leaks_proxy_password() {
        let p = "http://alice:s3cr3t@1.2.3.4:7890";
        // curl 真实输出（(5) Unsupported proxy syntax 时会带上整个代理 URL）
        let msg = "curl: (5) Unsupported proxy syntax in 'http://alice:s3cr3t@1.2.3.4:7890'";
        let out = scrub_proxy_in(msg, Some(p));
        assert!(!out.contains("s3cr3t"), "口令不得出现在错误文本里: {out}");
        assert!(
            out.contains("alice:***@1.2.3.4:7890"),
            "应替换成脱敏形式: {out}"
        );

        // 无关文本、没配代理、代理串不在文本里 → 原样返回
        assert_eq!(scrub_proxy_in("别的错误", Some(p)), "别的错误");
        assert_eq!(scrub_proxy_in("别的错误", None), "别的错误");
        assert_eq!(
            scrub_proxy_in("curl: (7) Failed to connect", Some(p)),
            "curl: (7) Failed to connect"
        );
    }

    #[test]
    fn proxy_redaction_hides_password_only() {
        assert_eq!(
            redact_proxy("socks5h://alice:s3cr3t@1.2.3.4:1080"),
            "socks5h://alice:***@1.2.3.4:1080"
        );
        // 无 userinfo / 无 scheme 时原样返回
        assert_eq!(redact_proxy("http://1.2.3.4:7890"), "http://1.2.3.4:7890");
        assert_eq!(redact_proxy(""), "");
    }

    #[test]
    fn proxy_is_split_into_three_fields_and_password_never_returned() {
        // 口令含 @ 与 :（最刁钻的情况）→ 服务端要能正确拆出「地址 / 账号 / 有没有口令」
        let full = compose_proxy(
            "socks5h://1.2.3.4:1080",
            build_userinfo("alice", "p@ss:word").as_deref(),
        )
        .unwrap();
        assert_eq!(full, "socks5h://alice:p%40ss%3Aword@1.2.3.4:1080");
        assert!(validate_proxy(&full).is_ok(), "拼出来的串必须过自己的校验");

        let (url, user, has_pw) = split_proxy(&full);
        assert_eq!(url, "socks5h://1.2.3.4:1080");
        assert_eq!(user, "alice");
        assert!(has_pw, "有口令，但口令本身绝不返回");
        // 脱敏串里也不能出现口令
        let red = redact_proxy(&full);
        assert!(
            !red.contains("p@ss") && !red.contains("p%40ss"),
            "脱敏漏了口令: {red}"
        );
    }

    #[test]
    fn proxy_without_credentials_roundtrip() {
        let full = compose_proxy("http://1.2.3.4:7890", None).unwrap();
        assert_eq!(full, "http://1.2.3.4:7890");
        let (url, user, has_pw) = split_proxy(&full);
        assert_eq!(url, "http://1.2.3.4:7890");
        assert!(user.is_empty());
        assert!(!has_pw);

        // 地址里自带 userinfo 一律拒绝（必须走独立的账号/口令框）
        assert!(compose_proxy("http://u:p@1.2.3.4:7890", None).is_err());
        assert!(compose_proxy("", None).is_err());
    }

    #[test]
    fn proxy_curl_args_add_proxy_only_when_set() {
        let plain = curl_args(None);
        assert!(
            !plain.iter().any(|a| a == "--proxy"),
            "未配代理时不传 --proxy（交给环境变量）"
        );
        assert!(plain.contains(&"--proto-redir".to_string()));

        let with = curl_args(Some("socks5h://1.2.3.4:1080"));
        let i = with
            .iter()
            .position(|a| a == "--proxy")
            .expect("应带 --proxy");
        assert_eq!(with[i + 1], "socks5h://1.2.3.4:1080");
        // 协议限制仍在（不能因为配了代理就放开 https 约束）
        assert!(with.contains(&"=https".to_string()));
    }
}
