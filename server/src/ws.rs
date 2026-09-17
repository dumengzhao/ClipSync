use crate::hub::{OutMsg, OUT_QUEUE_CAPACITY};
use crate::models::{ClientToServer, ServerToClient};
use crate::state::AppState;
use axum::extract::{State, WebSocketUpgrade};
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// 单条 WS 消息上限。客户端发的都是小 JSON 信令（含文件清单），256 KiB 足够宽裕；
/// 设上限是为了防超大帧把内存撑爆（axum 默认无上限）。
const MAX_WS_MESSAGE_BYTES: usize = 256 * 1024;
/// 同时在线 WS 连接上限（每连接一条任务 + 一条出站队列）。
const MAX_WS_CONNS: usize = 2048;
/// 空闲超时：客户端每 25s 发一次 Heartbeat，长时间收不到任何帧即视为死连接。
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// 当前在线 WS 连接数
static WS_CONNS: AtomicUsize = AtomicUsize::new(0);

/// 连接计数守卫：无论从哪个分支返回都会把计数减回去（避免计数只增不减）
struct ConnGuard;

impl Drop for ConnGuard {
    fn drop(&mut self) {
        WS_CONNS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 设备 WS 入口：/ws
pub async fn device_ws(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: axum::extract::ws::WebSocket, state: Arc<AppState>) {
    // 连接数上限：超出直接关闭，避免海量空闲连接耗尽内存与文件句柄
    if WS_CONNS.fetch_add(1, Ordering::SeqCst) >= MAX_WS_CONNS {
        WS_CONNS.fetch_sub(1, Ordering::SeqCst);
        eprintln!("[clipsync-server] WS 连接数已达上限（{MAX_WS_CONNS}），拒绝新连接");
        return;
    }
    let _conn_guard = ConnGuard;

    let (mut sender, mut receiver) = socket.split();
    // 有界出站队列：慢客户端不再让服务端推送无限堆积（满则 try_send 丢弃）
    let (tx, mut rx) = mpsc::channel::<OutMsg>(OUT_QUEUE_CAPACITY);

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
    loop {
        // 空闲超时：正常客户端每 25s 发一次 Heartbeat；长时间收不到任何帧说明
        // 连接已死（或对端只是占坑），主动断开，不让闲置连接长期占用资源。
        let msg = match tokio::time::timeout(WS_IDLE_TIMEOUT, receiver.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(_) => break,
            Err(_) => {
                eprintln!("[clipsync-server] WS 空闲超时（{WS_IDLE_TIMEOUT:?}），关闭连接");
                break;
            }
        };
        match msg {
            axum::extract::ws::Message::Text(t) => {
                let parsed: ClientToServer = match serde_json::from_str(&t) {
                    Ok(p) => p,
                    Err(_) => {
                        let _ = tx.try_send(OutMsg::App(ServerToClient::Error {
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
                                let _ = tx.try_send(msg);
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
                        // 回执（含未鉴权连接）：客户端读侧靠「90s 无入帧判死链」，没有回执
                        // 它无法区分「健康但空闲」与「中间链路静默假死」（2026-09-17 实际
                        // 发生：经代理链的 WS 被静默丢弃，客户端永久显示已连接）。
                        // try_send：控制消息可丢（队列满说明对端 socket 已堵，客户端正好
                        // 会经活性超时自行重连）。
                        let _ = tx.try_send(OutMsg::App(ServerToClient::HeartbeatAck));
                    }
                    ClientToServer::RelayText { to, ct } => {
                        if let Some((net, dev)) = &authed {
                            state.relay_text(net, dev, &to, &ct, &tx).await;
                        }
                    }
                    ClientToServer::FileNotify {
                        manifest,
                        ext_file_ep,
                    } => {
                        if let Some((net, dev)) = &authed {
                            state
                                .file_notify(net, dev, manifest, &ext_file_ep, &tx)
                                .await;
                        }
                    }
                }
            }
            axum::extract::ws::Message::Close(_) => break,
            axum::extract::ws::Message::Ping(_) => {
                let _ = tx.try_send(OutMsg::Pong);
            }
            _ => {}
        }
    }

    if let Some((_, dev)) = authed {
        state.disconnect(&dev);
    }
    forward.abort();
}
