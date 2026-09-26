use crate::crypto::{gen_token, hash_token, issue_session, verify_session};
use crate::models::Network;
use crate::state::AppState;
use crate::storage;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rust_embed::RustEmbed;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// 登录失败的退避策略参数。
///
/// 管理端只有一个账号，早期实现没有失败计数，可以无限次尝试口令——配合
/// **每请求一次 Argon2 校验**，既是爆破入口也是 CPU 消耗入口。
/// 这里做全局退避：连续失败到阈值即锁定，锁定时长逐次翻倍（有上限），
/// 登录成功立刻清零。
const LOGIN_MAX_FAILS: u32 = 5;
const LOGIN_BASE_LOCK: Duration = Duration::from_secs(30);
const LOGIN_MAX_LOCK: Duration = Duration::from_secs(900);

struct LoginThrottle {
    fails: u32,
    locked_until: Option<Instant>,
    lock_len: Duration,
}

/// 登录退避：**按源 IP** 各自计数。此前是全局退避，任意远程主机发 5 次
/// 错误口令即可让真实管理员被无限期锁死（锁死成本为零）。按 IP 区分后
/// 攻击者只能锁死自己；同一 NAT 后多用户互锁属可接受取舍。
/// 表项上限 1024，超出时清理已过期的锁定项（防内存被伪造源打爆）。
static LOGIN_THROTTLES: LazyLock<Mutex<HashMap<std::net::IpAddr, LoginThrottle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 单 IP 失败表上限：超过即做一轮过期清理（攻击者伪造海量源时防内存膨胀）
const LOGIN_THROTTLE_MAX_IPS: usize = 1024;

/// 解析 IP，容忍带端口的写法（`1.2.3.4:5678`）与两侧引号。
fn parse_ip_lax(raw: &str) -> Option<IpAddr> {
    let s = raw.trim().trim_matches('"');
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Some(ip);
    }
    // 仅有单个冒号时才按 host:port 拆（裸 IPv6 已在上面直接解析成功）
    let (host, _) = s.rsplit_once(':')?;
    host.trim().parse::<IpAddr>().ok()
}

/// 取「登录退避计数用的客户端 IP」。
///
/// 直连地址是**环回**或**受信代理**（`TRUSTED_PROXIES` 环境变量，逗号分隔 IP）时，
/// 说明请求来自已知反向代理，此时所有请求的直连地址都相同——按它计数会让退避
/// 退化成单桶（同机 nginx 时是 127.0.0.1；异机代理时是所有用户共享代理 IP，
/// 任意人 5 次错码即可锁死管理员，比修复前更糟）。此时改看代理写入的转发头：
/// - 优先 `X-Real-IP`：代理 `proxy_set_header X-Real-IP $remote_addr` 会**覆盖**
///   客户端传入值，不可伪造；
/// - 退而取 `X-Forwarded-For` 的**最后一段**：`$proxy_add_x_forwarded_for`
///   把真实来源追加在末尾，取第一段会被客户端预置的假值欺骗。
///
/// 直连地址既非环回也不在受信列表（客户端直连 20070，或**未经声明**的代理）时
/// **一律忽略转发头**：那些头在公网可任意伪造，采信等于把退避键交给攻击者选择。
/// 注意「异机反代」必须在 `TRUSTED_PROXIES` 里显式声明代理出口 IP 才会被采信——
/// 2026-09-17 实测的生产形态就是异机 1Panel/OpenResty（如 203.0.113.20）→ 源站，
/// 只认环回会让该路径退回单桶。
fn client_throttle_key(
    peer: Option<SocketAddr>,
    headers: &axum::http::HeaderMap,
    trusted: &[IpAddr],
) -> IpAddr {
    let direct = peer.map(|p| p.ip());
    if direct.is_some_and(|ip| ip.is_loopback() || trusted.contains(&ip)) {
        if let Some(ip) = headers
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_ip_lax)
        {
            return ip;
        }
        if let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.rsplit(',').next())
            .and_then(parse_ip_lax)
        {
            return ip;
        }
        // 来自环回/受信代理但**没有任何转发头**：反代没配 proxy_set_header X-Real-IP /
        // X-Forwarded-For。此时所有经代理的登录都塌缩到同一个键，退避退化成单桶——
        // 任意人 5 次错码即可锁死管理员（这正是本函数要解决的问题）。这种配置疏漏
        // 在行为上很难发现（锁仍然"工作"，只是分桶错了），所以必须主动告警一次。
        warn_missing_forward_headers_once();
    }
    direct.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

/// 只在首次遇到「受信来源但无转发头」时告警一次，避免刷日志。
static MISSING_FORWARD_HEADER_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn warn_missing_forward_headers_once() {
    if !MISSING_FORWARD_HEADER_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            "请求来自环回/受信代理但未携带 X-Real-IP / X-Forwarded-For —— \
             反向代理没有转发真实客户端 IP，管理登录退避会退化成单桶 \
             （任意来源 5 次错码即可锁死管理员）。请在反代补上：\
             proxy_set_header X-Real-IP $remote_addr; \
             proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;"
        );
    }
}

/// 内嵌管理页面资源（编译时打包进二进制，免部署静态文件）。
///
/// `pub(crate)`：下载页（`downloads.html`，在 `update.rs` 里提供，**无需登录**）
/// 也用同一份内嵌资源。
#[derive(RustEmbed)]
#[folder = "static/"]
pub(crate) struct Assets;

/// 取一份凭据快照：`None` = 尚未初始化。
///
/// 别跨 await 持锁：这里没有 await，但保持同样的习惯。
fn creds_snapshot(state: &AppState) -> Option<storage::AdminCreds> {
    state
        .admin_creds
        .lock()
        .map(|c| c.clone())
        .unwrap_or_else(|e| e.into_inner().clone())
}

/// 「尚未初始化」的统一响应：409 + `code`，前端据此切到初始化页面。
fn not_initialized() -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({"error": "管理员凭据尚未初始化", "code": "not_initialized"})),
    )
        .into_response()
}

/// 管理 API 鉴权中间件：校验 Bearer 会话 token（HMAC 签名）。
///
/// 未初始化时一律 409 —— 此时根本没有凭据可校验，必须明确区分「没初始化」和
/// 「token 不对」，否则前端只会看到「未授权」然后反复弹登录框。
pub async fn admin_auth(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let Some(creds) = creds_snapshot(&state) else {
        return not_initialized();
    };
    let token = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let ok = token
        .map(|t| verify_session(&state.server_key, t, creds.updated_at).is_some())
        .unwrap_or(false);
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    next.run(req).await
}

#[derive(serde::Deserialize)]
pub struct CreateNetBody {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(serde::Deserialize)]
pub struct RenameNetBody {
    pub name: String,
}

#[derive(serde::Deserialize)]
pub struct LoginBody {
    pub user: String,
    pub pass: String,
}

/// 登录：校验密码后签发标准 JWT 会话令牌（HS256，7 天有效，无状态）。
pub async fn admin_login(
    State(state): State<Arc<AppState>>,
    // ConnectInfo 可选：单测的 oneshot 请求没有对端地址，此时退避退化为全局键
    peer_addr: Option<ConnectInfo<std::net::SocketAddr>>,
    // 反代场景下真实来源在转发头里，取键规则见 client_throttle_key：
    // 仅环回或 TRUSTED_PROXIES 声明的受信代理才采信转发头。
    headers: axum::http::HeaderMap,
    Json(body): Json<LoginBody>,
) -> Response {
    let Some(creds) = creds_snapshot(&state) else {
        return not_initialized();
    };
    let throttle_key =
        client_throttle_key(peer_addr.map(|c| c.0), &headers, &state.trusted_proxies);
    // 1) 退避检查（按源 IP）：锁定期内直接拒绝，且**不做口令校验**（也就不会消耗 Argon2）
    {
        let t = LOGIN_THROTTLES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(th) = t.get(&throttle_key) {
            if let Some(until) = th.locked_until {
                let now = Instant::now();
                if now < until {
                    let left = (until - now).as_secs() + 1;
                    return Json(json!({
                        "error": format!("登录尝试过于频繁，请 {left} 秒后再试")
                    }))
                    .into_response();
                }
            }
        }
    }
    if body.user != creds.user || !storage::verify_pass(&creds.pass_hash, &body.pass) {
        // 2) 失败计数与递增锁定
        {
            let mut t = LOGIN_THROTTLES.lock().unwrap_or_else(|e| e.into_inner());
            if t.len() >= LOGIN_THROTTLE_MAX_IPS {
                // 防伪造源撑爆内存：清掉已过期的锁定项
                let now = Instant::now();
                t.retain(|_, th| th.locked_until.map(|u| now < u).unwrap_or(true));
            }
            let th = t.entry(throttle_key).or_insert_with(|| LoginThrottle {
                fails: 0,
                locked_until: None,
                lock_len: LOGIN_BASE_LOCK,
            });
            th.fails = th.fails.saturating_add(1);
            if th.fails >= LOGIN_MAX_FAILS {
                let now = Instant::now();
                th.locked_until = Some(now + th.lock_len);
                tracing::warn!(
                    "{throttle_key} 管理登录连续失败 {} 次，锁定 {:?}",
                    th.fails,
                    th.lock_len
                );
                th.lock_len = (th.lock_len * 2).min(LOGIN_MAX_LOCK);
                th.fails = 0;
            }
        }
        return Json(json!({"error": "invalid credentials"})).into_response();
    }
    // 3) 成功：清零退避状态
    {
        // 成功即清除该 IP 的退避状态
        LOGIN_THROTTLES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&throttle_key);
    }
    let token = issue_session(&state.server_key, &creds.user, creds.updated_at);
    Json(json!({ "token": token })).into_response()
}

#[derive(serde::Deserialize)]
pub struct InitBody {
    /// 管理员用户名
    pub user: String,
    /// **口令哈希串**（不是口令本身）：管理员在别处用 Argon2id 生成后粘贴过来
    pub pass_hash: String,
}

/// 管理员用户名长度上限。
///
/// 初始化端点是**公开**的（未初始化时谁都能调），所以每个入参都要有上限 ——
/// 否则可以在窗口期塞一个几 MB 的用户名进 `admin.json`，之后每次读凭据都难受。
const ADMIN_USER_MAX_LEN: usize = 64;
/// 新口令长度上限（正常口令远小于此；防止已登录者用超长输入拖慢 Argon2）。
const ADMIN_PASS_MAX_LEN: usize = 1024;

/// GET /api/admin/init-status —— 是否已经初始化（**公开**端点）。
///
/// 公开是因为它只回答「有没有凭据」这一个比特，而「没有凭据」这件事对任何访问者
/// 都是显然的（届时所有管理接口都返回同一个 409）。前端据此决定显示登录卡片还是
/// 初始化卡片，省得用户在未初始化时对着登录框干瞪眼。
pub async fn admin_init_status(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({ "initialized": creds_snapshot(&state).is_some() }))
}

/// POST /api/admin/init —— **仅在未初始化时**可用：写入用户名与口令哈希串。
///
/// 设计取舍：
/// - 只收**哈希串**、不收明文口令：明文从生成到使用都不经过服务端；
/// - 只能做一次（已初始化即 409）—— 否则谁都能抢先给自己设个口令；
///   判据是**内存态 + 磁盘各查一次**：内存态挡住正常路径，磁盘那次挡住
///   「运行期间手工放进 admin.json」这种两边不一致的情况（见函数内的注释）；
/// - 写入后**不签发会话**：管理员必须用真实口令登录一次，能登进去才证明他贴的这串
///   哈希确实对应他知道的那个口令。否则贴错一串就等于把自己永久锁在门外（只能去
///   服务器上删 admin.json）。
pub async fn admin_init(
    State(state): State<Arc<AppState>>,
    Json(body): Json<InitBody>,
) -> Response {
    let user = body.user.trim().to_string();
    if user.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "用户名不能为空"})),
        )
            .into_response();
    }
    if user.chars().count() > ADMIN_USER_MAX_LEN {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("用户名过长（上限 {ADMIN_USER_MAX_LEN} 字符）")})),
        )
            .into_response();
    }
    // 控制字符一律拒绝：这个值会写进 admin.json、也会进日志与页面，
    // 带换行就能伪造日志行、把管理页的显示搅乱。
    if user.chars().any(|c| c.is_control()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "用户名不能包含换行等控制字符"})),
        )
            .into_response();
    }
    // 哈希校验放在取锁之前：它要跑一次 Argon2（慢），别占着锁
    let hash = match storage::validate_hash_input(&body.pass_hash) {
        Ok(h) => h,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    };
    // 检查与写入在同一个临界区内，避免两个并发请求都通过「未初始化」检查
    let mut guard = state.admin_creds.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "管理员凭据已初始化", "code": "already_initialized"})),
        )
            .into_response();
    }
    // 二次确认：**磁盘上真的没有凭据文件**。
    //
    // 内存态是启动时读进来的，两者可能不一致 —— 比如运行期间有人（部署脚本）手工
    // 放了一份 admin.json，内存里还是 None。只看内存的话，这个请求就会把那份文件
    // **悄悄覆盖掉**，管理员还以为自己配的口令生效了。
    // 顺手把内存态补成磁盘内容，状态就自愈了。
    if let Some(on_disk) = state.store.load_admin() {
        *guard = Some(on_disk);
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "凭据文件已存在，不能覆盖（请用真实密码登录，或直接改数据目录下的 admin.json）",
                "code": "already_initialized"
            })),
        )
            .into_response();
    }
    let creds = storage::AdminCreds {
        user: user.clone(),
        pass_hash: hash,
        updated_at: storage::now_unix(),
    };
    if let Err(e) = state.store.save_admin(&creds) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("写入 admin.json 失败：{e}")})),
        )
            .into_response();
    }
    *guard = Some(creds);
    tracing::info!(
        "管理员凭据已初始化（{}）——请用真实密码登录一次以验证哈希串",
        crate::logging::clean(&user)
    );
    Json(json!({"ok": true, "user": user})).into_response()
}

#[derive(serde::Deserialize)]
pub struct ChangePassBody {
    /// 当前密码（必须提供：防止拿到会话就能改密码）
    pub old_pass: String,
    pub new_pass: String,
}

/// POST /api/admin/password —— 修改管理员密码。
///
/// 要点：① 必须验旧口令；② 长度与强度校验沿用启动时的规则（≥8 位、不许是 `clipsync`）；
/// ③ 只把 **Argon2id 哈希**写进 admin.json（0600），明文不落盘；
/// ④ 更新密码版本号 → **所有已签发的会话令牌立即失效**，前端改完必须重新登录。
pub async fn change_password(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ChangePassBody>,
) -> Response {
    let new = body.new_pass.trim();
    if new.len() > ADMIN_PASS_MAX_LEN {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("新密码过长（上限 {ADMIN_PASS_MAX_LEN} 字符）")})),
        )
            .into_response();
    }
    if let Err(e) = storage::validate_new_password(new) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
    }
    let updated = match state.store.change_password(&body.old_pass, new) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    // 内存态同步：改完密码，新哈希与版本号都要立刻生效（否则新密码登不进来、
    // 旧令牌也还在用）
    let user = {
        let mut cur = state.admin_creds.lock().unwrap_or_else(|e| e.into_inner());
        *cur = Some(updated.clone());
        updated.user.clone()
    };
    tracing::info!(
        "管理员密码已更新（{}）——旧会话令牌已全部失效，需要重新登录",
        crate::logging::clean(&user)
    );
    Json(json!({"ok": true})).into_response()
}

/// GET /admin：返回内嵌的管理页面 HTML。
pub async fn admin_page() -> Response {
    match Assets::get("admin.html") {
        Some(f) => (
            StatusCode::OK,
            [(CONTENT_TYPE, "text/html; charset=utf-8")],
            f.data.to_vec(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "admin page not embedded".to_string()).into_response(),
    }
}

/// GET /admin/static/*：返回内嵌的其它静态资源（js/css 等）。
pub async fn admin_static(Path(p): Path<String>) -> Response {
    match Assets::get(&p) {
        Some(f) => {
            let ct: &str = if p.ends_with(".js") {
                "application/javascript"
            } else if p.ends_with(".css") {
                "text/css"
            } else {
                "application/octet-stream"
            };
            (StatusCode::OK, [(CONTENT_TYPE, ct)], f.data.to_vec()).into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found".to_string()).into_response(),
    }
}

pub async fn list_networks(State(state): State<Arc<AppState>>) -> Json<Value> {
    let nets = state.networks.lock().unwrap();
    let out: Vec<Value> = nets
        .iter()
        .map(|n| {
            json!({
                "id": n.id,
                "name": n.name,
                "description": n.description,
                "token": n.token,
                "created": n.created,
                "node_count": n.nodes.len(),
                "enabled_count": n.nodes.iter().filter(|x| x.enabled).count(),
            })
        })
        .collect();
    Json(json!({ "networks": out }))
}

pub async fn create_network(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateNetBody>,
) -> Json<Value> {
    let token = gen_token();
    let net = Network {
        id: gen_token(),
        token_hash: hash_token(&token),
        token: token.clone(),
        name: body.name,
        description: body.description,
        nodes: vec![],
        removed_devices: vec![],
        created: crate::state::now_secs(),
    };
    {
        let mut nets = state.networks.lock().unwrap();
        nets.push(net.clone());
    }
    let _ = state.save();
    // Token 仅在此处明文返回一次
    Json(json!({ "id": net.id, "name": net.name, "token": token }))
}

pub async fn rename_network(
    State(state): State<Arc<AppState>>,
    Path(net_id): Path<String>,
    Json(body): Json<RenameNetBody>,
) -> Json<Value> {
    let new_name = body.name.trim().to_string();
    if new_name.is_empty() {
        return Json(json!({ "error": "name required" }));
    }
    let found = {
        let mut nets = state.networks.lock().unwrap();
        match nets.iter_mut().find(|n| n.id == net_id) {
            Some(n) => {
                n.name = new_name;
                true
            }
            None => false,
        }
    };
    if !found {
        return Json(json!({ "error": "not found" }));
    }
    let _ = state.save();
    Json(json!({ "ok": true }))
}

pub async fn list_devices(
    State(state): State<Arc<AppState>>,
    Path(net_id): Path<String>,
) -> Json<Value> {
    let nets = state.networks.lock().unwrap();
    let net = match nets.iter().find(|n| n.id == net_id) {
        Some(n) => n,
        None => return Json(json!({"error": "not found"})),
    };
    let nodes: Vec<Value> = net
        .nodes
        .iter()
        .map(|n| {
            json!({
                "device_id": n.device_id,
                "name": n.name,
                "lan_group": n.lan_group,
                "ext_file_ep": n.ext_file_ep,
                "platform": n.platform,
                "hardware_id": n.hardware_id,
                "os_version": n.os_version,
                "enabled": n.enabled,
                "online": n.online,
                "last_seen": n.last_seen,
            })
        })
        .collect();
    Json(json!({ "nodes": nodes }))
}

pub async fn enable_device_handler(
    State(state): State<Arc<AppState>>,
    Path((net_id, dev_id)): Path<(String, String)>,
) -> Json<Value> {
    if state.enable_device(&net_id, &dev_id) {
        Json(json!({"ok": true}))
    } else {
        Json(json!({"error": "not found"}))
    }
}

pub async fn disable_device_handler(
    State(state): State<Arc<AppState>>,
    Path((net_id, dev_id)): Path<(String, String)>,
) -> Json<Value> {
    if state.disable_device(&net_id, &dev_id) {
        Json(json!({"ok": true}))
    } else {
        Json(json!({"error": "not found"}))
    }
}

pub async fn remove_device_handler(
    State(state): State<Arc<AppState>>,
    Path((net_id, dev_id)): Path<(String, String)>,
) -> Json<Value> {
    if state.remove_device(&net_id, &dev_id) {
        Json(json!({"ok": true}))
    } else {
        Json(json!({"error": "not found"}))
    }
}

/// GET /api/admin/networks/:id/removed —— 列出该网络黑名单（已移除）设备。
pub async fn list_removed_handler(
    State(state): State<Arc<AppState>>,
    Path(net_id): Path<String>,
) -> Json<Value> {
    let removed = state.removed_devices(&net_id);
    Json(json!({ "removed": removed }))
}

/// POST /api/admin/networks/:id/removed/:dev/restore —— 从黑名单移除（恢复），允许其重新配对入网。
pub async fn restore_device_handler(
    State(state): State<Arc<AppState>>,
    Path((net_id, dev_id)): Path<(String, String)>,
) -> Json<Value> {
    if state.restore_device(&net_id, &dev_id) {
        Json(json!({"ok": true}))
    } else {
        Json(json!({"error": "not found"}))
    }
}

/// POST /api/admin/networks/:id/removed/:dev/purge —— 彻底删除该设备记录
/// （从黑名单永久移除，并清掉节点表里该 id 的任何残留）。用于清理废弃设备/旧身份。
pub async fn purge_device_handler(
    State(state): State<Arc<AppState>>,
    Path((net_id, dev_id)): Path<(String, String)>,
) -> Json<Value> {
    if state.purge_device(&net_id, &dev_id) {
        Json(json!({"ok": true}))
    } else {
        Json(json!({"error": "not found"}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    fn sock(addr: &str) -> Option<SocketAddr> {
        Some(addr.parse().unwrap())
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    /// 直连（非环回）时**必须**忽略转发头，否则公网可直接伪造来源把退避键
    /// 指向管理员 IP（或随意分散以绕过计数）。
    #[test]
    fn direct_peer_ignores_forward_headers() {
        let key = client_throttle_key(
            sock("203.0.113.9:5001"),
            &headers(&[("x-real-ip", "198.51.100.7")]),
            &[],
        );
        assert_eq!(key.to_string(), "203.0.113.9");
    }

    /// 经 nginx（直连为环回）时取 X-Real-IP：否则所有登录共用 127.0.0.1 一个桶，
    /// 任意人 5 次错码即可锁死管理员。
    #[test]
    fn proxied_peer_uses_x_real_ip() {
        let key = client_throttle_key(
            sock("127.0.0.1:5002"),
            &headers(&[("x-real-ip", "203.0.113.9")]),
            &[],
        );
        assert_eq!(key.to_string(), "203.0.113.9");
    }

    /// 没有 X-Real-IP 时取 XFF 的**最后一段**：首段可被客户端预置伪造。
    #[test]
    fn proxied_peer_uses_last_forwarded_hop() {
        let key = client_throttle_key(
            sock("[::1]:5003"),
            &headers(&[("x-forwarded-for", "1.2.3.4, 203.0.113.9")]),
            &[],
        );
        assert_eq!(key.to_string(), "203.0.113.9");
    }

    /// 环回但不带任何转发头（本机 curl / 端口转发）→ 退化为环回地址本身。
    #[test]
    fn loopback_without_headers_falls_back_to_loopback() {
        let key = client_throttle_key(sock("127.0.0.1:5004"), &HeaderMap::new(), &[]);
        assert!(key.is_loopback());
    }

    /// 单测的 oneshot 请求没有对端地址（ConnectInfo 缺失）→ 全局键，行为与修复前一致。
    #[test]
    fn missing_peer_addr_is_unspecified() {
        let key = client_throttle_key(None, &HeaderMap::new(), &[]);
        assert_eq!(key.to_string(), "0.0.0.0");
    }

    /// 异机受信代理（TRUSTED_PROXIES 声明，如 1Panel/OpenResty 独立主机）：
    /// 采信转发头取真实客户端 IP——否则所有经代理的登录共享代理 IP 一个桶，
    /// 任意人 5 次错码即可锁死全部管理员会话（2026-09-17 生产实测形态）。
    #[test]
    fn trusted_proxy_peer_uses_forward_headers() {
        let trusted: Vec<IpAddr> = vec!["198.51.100.1".parse().unwrap()];
        let key = client_throttle_key(
            sock("198.51.100.1:5005"),
            &headers(&[("x-real-ip", "203.0.113.9")]),
            &trusted,
        );
        assert_eq!(key.to_string(), "203.0.113.9");
        // X-Real-IP 缺失时退化 XFF 末段
        let key = client_throttle_key(
            sock("198.51.100.1:5006"),
            &headers(&[("x-forwarded-for", "1.2.3.4, 203.0.113.9")]),
            &trusted,
        );
        assert_eq!(key.to_string(), "203.0.113.9");
    }

    /// 未在 TRUSTED_PROXIES 声明的非环回来源：哪怕带着转发头也一律忽略
    /// （公网可伪造来源头，采信等于把退避键交给攻击者）。
    #[test]
    fn untrusted_peer_ignores_forward_headers() {
        let trusted: Vec<IpAddr> = vec!["198.51.100.1".parse().unwrap()];
        let key = client_throttle_key(
            sock("203.0.113.9:5007"),
            &headers(&[("x-real-ip", "198.51.100.7")]),
            &trusted,
        );
        assert_eq!(key.to_string(), "203.0.113.9");
    }

    /// 带端口的转发头写法（部分反代如此）也要能解析。
    #[test]
    fn parse_ip_lax_accepts_port_and_quotes() {
        assert_eq!(
            parse_ip_lax("\"203.0.113.9:8080\"").unwrap().to_string(),
            "203.0.113.9"
        );
        assert_eq!(
            parse_ip_lax(" 2001:db8::1 ").unwrap().to_string(),
            "2001:db8::1"
        );
        assert!(parse_ip_lax("not-an-ip").is_none());
    }
}

/// 初始化端点的回归测试：重点是把「**已初始化就绝对不能再覆盖**」钉死。
#[cfg(test)]
mod init_tests {
    use super::*;
    use std::sync::Mutex;

    fn tmp_state(creds: Option<storage::AdminCreds>) -> (Arc<AppState>, std::path::PathBuf) {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("clipsync-init-ut-{}-{uniq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState {
            store: storage::Store::new(dir.clone()),
            networks: Mutex::new(vec![]),
            hub: crate::hub::Hub::new(),
            admin_ws: Mutex::new(HashMap::new()),
            server_key: "test-key".into(),
            admin_creds: Mutex::new(creds),
            update_dir: dir.join("update"),
            update_public_base: None,
            update_max_upload: 10 * 1024 * 1024,
            trusted_proxies: vec![],
        });
        (state, dir)
    }

    fn on_disk_hash(state: &AppState) -> Option<String> {
        state.store.load_admin().map(|c| c.pass_hash)
    }

    /// 场景一：内存已是「已初始化」→ 409，磁盘原封不动。
    #[tokio::test]
    async fn refuses_when_memory_says_initialized() {
        let original = storage::hash_pass("original-pw");
        let (state, _dir) = tmp_state(Some(storage::AdminCreds {
            user: "admin".into(),
            pass_hash: original.clone(),
            updated_at: 1,
        }));
        state
            .store
            .save_admin(&storage::AdminCreds {
                user: "admin".into(),
                pass_hash: original.clone(),
                updated_at: 1,
            })
            .unwrap();

        let resp = admin_init(
            State(state.clone()),
            Json(InitBody {
                user: "hacker".into(),
                pass_hash: storage::hash_pass("evil-pw"),
            }),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(
            on_disk_hash(&state),
            Some(original),
            "磁盘上的凭据必须纹丝不动"
        );
    }

    /// 场景二：内存说没初始化，但磁盘上已经有凭据文件（运行期间被手工放进去的）→
    /// 同样 409，并且**不能覆盖**那份文件（否则管理员配的口令会被页面悄悄改写）。
    #[tokio::test]
    async fn refuses_when_credentials_file_exists_on_disk() {
        let original = storage::hash_pass("handwritten-pw");
        let (state, _dir) = tmp_state(None); // 内存：未初始化
        state
            .store
            .save_admin(&storage::AdminCreds {
                user: "admin".into(),
                pass_hash: original.clone(),
                updated_at: 1,
            })
            .unwrap();

        let resp = admin_init(
            State(state.clone()),
            Json(InitBody {
                user: "hacker".into(),
                pass_hash: storage::hash_pass("evil-pw"),
            }),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(
            on_disk_hash(&state),
            Some(original),
            "不能覆盖磁盘上已有的凭据"
        );
        // 顺便自愈：内存态补成磁盘内容，之后不用重启也能正常登录
        assert!(creds_snapshot(&state).is_some(), "内存态应被补成磁盘内容");
    }

    /// 场景三：真的没初始化 → 第一次成功，第二次必定被拒且内容不变。
    #[tokio::test]
    async fn succeeds_once_then_locks_out() {
        let (state, _dir) = tmp_state(None);
        let mine = storage::hash_pass("my-real-pw");

        let resp = admin_init(
            State(state.clone()),
            Json(InitBody {
                user: "admin".into(),
                pass_hash: mine.clone(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(on_disk_hash(&state), Some(mine.clone()));

        let resp2 = admin_init(
            State(state.clone()),
            Json(InitBody {
                user: "hacker".into(),
                pass_hash: storage::hash_pass("evil-pw"),
            }),
        )
        .await;
        assert_eq!(resp2.status(), StatusCode::CONFLICT);
        assert_eq!(
            on_disk_hash(&state),
            Some(mine),
            "第二次不得改写已写入的凭据"
        );
    }
}
