use crate::hub::OutMsg;
use crate::models::{ClientToServer, ServerToClient};
use crate::state::AppState;
use axum::extract::{State, WebSocketUpgrade};
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::mpsc;

/// 设备 WS 入口：/ws
pub async fn device_ws(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: axum::extract::ws::WebSocket, state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<OutMsg>();

    // 转发任务：把服务端要发的消息写到 WS（App 消息序列化为 Text；Ping 回 Pong）
    let forward = tokio::spawn(async move {
        while let Some(out) = rx.recv().await {
            match out {
                OutMsg::App(msg) => {
                    let text = match serde_json::to_string(&msg) {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    if sender
                        .send(axum::extract::ws::Message::Text(text))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                OutMsg::Pong => {
                    if sender
                        .send(axum::extract::ws::Message::Pong(vec![]))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        // 队列耗尽后优雅发送 Close 帧：确保排队的应用消息（如拉黑后的 Removed）
        // 在 TCP 拆除前已送达对端，避免被 RST 直接丢弃导致客户端收不到。
        let _ = sender.send(axum::extract::ws::Message::Close(None)).await;
    });

    let mut authed: Option<(String, String)> = None; // (network_id, device_id)
    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            axum::extract::ws::Message::Text(t) => {
                let parsed: ClientToServer = match serde_json::from_str(&t) {
                    Ok(p) => p,
                    Err(_) => {
                        let _ = tx.send(OutMsg::App(ServerToClient::Error {
                            code: "bad_json".into(),
                            msg: "invalid json".into(),
                        }));
                        continue;
                    }
                };
                match parsed {
                    ClientToServer::Auth { token, device } => {
                        if authed.is_some() {
                            continue; // 已鉴权，忽略重复 auth
                        }
                        match state.handle_auth(&token, &device, &tx) {
                            Ok((net_id, dev_id)) => {
                                authed = Some((net_id, dev_id));
                            }
                            Err(e) => {
                                // 被拉黑的设备（device_removed）下发 Removed 让客户端明确停止重连
                                let msg = if e == "device_removed" {
                                    OutMsg::App(ServerToClient::Removed)
                                } else {
                                    OutMsg::App(ServerToClient::Error {
                                        code: "bad_token".into(),
                                        msg: e,
                                    })
                                };
                                // 先把拒绝消息送入转发队列，再优雅关闭转发任务，
                                // 确保 Removed/Error 真正刷到 socket（否则 forward.abort()
                                // 会在消息发出前杀掉转发任务，客户端收不到）。
                                let _ = tx.send(msg);
                                drop(tx);
                                let _ = forward.await;
                                return;
                            }
                        }
                    }
                    ClientToServer::Heartbeat => {
                        if let Some((_, dev)) = &authed {
                            state.touch(dev);
                        }
                    }
                    ClientToServer::RelayText { to, ct } => {
                        if let Some((net, dev)) = &authed {
                            state.relay_text(net, dev, &to, &ct, &tx);
                        }
                    }
                    ClientToServer::FileNotify {
                        manifest,
                        ext_file_ep,
                    } => {
                        if let Some((net, dev)) = &authed {
                            state.file_notify(net, dev, manifest, &ext_file_ep, &tx);
                        }
                    }
                }
            }
            axum::extract::ws::Message::Close(_) => break,
            axum::extract::ws::Message::Ping(_) => {
                let _ = tx.send(OutMsg::Pong);
            }
            _ => {}
        }
    }

    if let Some((_, dev)) = authed {
        state.disconnect(&dev);
    }
    forward.abort();
}
