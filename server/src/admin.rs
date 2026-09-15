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

/// 内嵌管理页面资源（编译时打包进二进制，免部署静态文件）。
#[derive(RustEmbed)]
#[folder = "static/"]
struct Assets;

/// 管理 API 鉴权中间件：校验 Bearer 会话 token（HMAC 签名）。
pub async fn admin_auth(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let ok = token
        .map(|t| verify_session(&state.server_key, t).is_some())
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
    Json(body): Json<LoginBody>,
) -> Json<Value> {
    let throttle_key = peer_addr
        .map(|c| c.0.ip())
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
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
                    }));
                }
            }
        }
    }
    if body.user != state.admin_user || !storage::verify_pass(&state.admin_pass_hash, &body.pass) {
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
                eprintln!(
                    "[clipsync-server] {throttle_key} 管理登录连续失败 {} 次，锁定 {:?}",
                    th.fails, th.lock_len
                );
                th.lock_len = (th.lock_len * 2).min(LOGIN_MAX_LOCK);
                th.fails = 0;
            }
        }
        return Json(json!({"error": "invalid credentials"}));
    }
    // 3) 成功：清零退避状态
    {
        // 成功即清除该 IP 的退避状态
        LOGIN_THROTTLES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&throttle_key);
    }
    let token = issue_session(&state.server_key, &state.admin_user);
    Json(json!({ "token": token }))
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
