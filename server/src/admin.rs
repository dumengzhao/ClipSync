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
/// 2026-09-17 实测的生产形态就是异机 1Panel/OpenResty（203.0.113.20）→ 源站，
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
        eprintln!(
            "[clipsync-server] 警告：请求来自环回/受信代理但未携带 X-Real-IP / X-Forwarded-For —— \
             反向代理没有转发真实客户端 IP，管理登录退避会退化成单桶 \
             （任意来源 5 次错码即可锁死管理员）。请在反代补上：\
             proxy_set_header X-Real-IP $remote_addr; \
             proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;"
        );
    }
}

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
    // 反代场景下真实来源在转发头里，取键规则见 client_throttle_key：
    // 仅环回或 TRUSTED_PROXIES 声明的受信代理才采信转发头。
    headers: axum::http::HeaderMap,
    Json(body): Json<LoginBody>,
) -> Json<Value> {
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
