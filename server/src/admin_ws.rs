use crate::crypto::verify_session;
use crate::state::AppState;
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Deserialize)]
pub struct AdminWsQuery {
    token: String,
    net_id: String,
}

/// 管理后台实时刷新 WebSocket：/api/admin/ws?token=<JWT>&net_id=<id>
/// 鉴权通过后保持连接，节点状态变化由 state.push_admin_nodes 主动推送完整列表。
pub async fn admin_ws(
    ws: WebSocketUpgrade,
    Query(q): Query<AdminWsQuery>,
    State(state): State<Arc<AppState>>,
) -> Response {
    // 自鉴权（WS 升级请求无法走 admin_auth 中间件的 Bearer 头）
    if verify_session(&state.server_key, &q.token).is_none() {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let net_id = q.net_id;
    ws.on_upgrade(move |socket| handle_admin(socket, state, net_id))
}

async fn handle_admin(
    socket: axum::extract::ws::WebSocket,
    state: Arc<AppState>,
    net_id: String,
) {
    let (mut sender, mut receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let tx = Arc::new(tx);
    state.register_admin_ws(&net_id, tx.clone());
    // 连接即推一次当前快照
    state.push_admin_nodes(&net_id);

    // 控制通道：主循环收到 Ping 后通知转发任务回 Pong（sender 归转发任务独占，避免借用冲突）
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<()>();

    let fwd = tokio::spawn(async move {
        loop {
            tokio::select! {
                s = rx.recv() => {
                    match s {
                        Some(s) => {
                            if sender
                                .send(axum::extract::ws::Message::Text(s))
                                .await
                                .is_err()
                            { break; }
                        }
                        None => break,
                    }
                }
                c = ctrl_rx.recv() => {
                    match c {
                        Some(_) => {
                            if sender
                                .send(axum::extract::ws::Message::Pong(vec![]))
                                .await
                                .is_err()
                            { break; }
                        }
                        None => break,
                    }
                }
            }
        }
        let _ = sender.send(axum::extract::ws::Message::Close(None)).await;
    });

    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            axum::extract::ws::Message::Close(_) => break,
            axum::extract::ws::Message::Ping(_) => {
                let _ = ctrl_tx.send(());
            }
            _ => {}
        }
    }
    fwd.abort();
    state.unregister_admin_ws(&net_id, &tx);
}
