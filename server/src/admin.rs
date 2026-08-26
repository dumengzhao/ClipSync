use crate::crypto::{gen_token, hash_token};
use crate::models::Network;
use crate::state::AppState;
use crate::storage;
use axum::extract::{Path, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

/// 管理 API 鉴权中间件（阶段1：Bearer ADMIN_API_KEY；阶段2 将替换为登录会话）。
pub async fn admin_auth(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.strip_prefix("Bearer ")
                .map(|t| t == state.admin_api_key)
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if !ok {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response();
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
pub struct LoginBody {
    pub user: String,
    pub pass: String,
}

/// 登录：校验密码后返回 admin_api_key（阶段2 改为签发会话 Cookie/JWT）。
pub async fn admin_login(State(state): State<Arc<AppState>>, Json(body): Json<LoginBody>) -> Json<Value> {
    if body.user != state.admin_user || !storage::verify_pass(&state.admin_pass_hash, &body.pass) {
        return Json(json!({"error": "invalid credentials"}));
    }
    Json(json!({ "token": state.admin_api_key }))
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
        name: body.name,
        description: body.description,
        nodes: vec![],
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
