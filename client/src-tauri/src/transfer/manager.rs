//! 传输连接管理器
//!
//! 负责把整条同步链路接通：
//! - 在 `listen_port` 上监听入站 WebSocket（作为 SPAKE2 应答方）
//! - 发现对端后主动发起 WebSocket 连接（作为 SPAKE2 发起方）
//! - 连接建立后跑 SPAKE2 配对握手，派生会话密钥
//! - 用 AES-GCM 加密通道传输剪贴板同步包（`SyncEnvelope`）
//! - 本地剪贴板变化经引擎广播后，加密转发给所有已连接对端
//! - 收到对端同步包解密后写回本地剪贴板（含防回环）

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Listener};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::clipboard::types::{ClipboardContent, SyncMark};
use crate::crypto::aead::{decrypt, encrypt};
use crate::crypto::kdf::{derive_session_keys, split_keys};
use crate::crypto::pake::{start_initiator, start_responder};
use crate::device::identity::DeviceIdentity;
use crate::discovery::DiscoveredPeer;
use crate::sync::engine::{SyncEngine, SyncEvent};
use crate::transfer::websocket::{MessageFrame, MessageType};

/// 跨连接传输的剪贴板信封（加密前 / 解密后）
#[derive(Clone, Serialize, Deserialize)]
pub struct SyncEnvelope {
    pub mark: SyncMark,
    pub content: ClipboardContent,
}

#[derive(Clone, Copy)]
enum Role {
    Initiator,
    Responder,
}

/// 单个已连接对端（用于把本地剪贴板变化转发给它）
#[derive(Clone)]
struct Peer {
    tx: mpsc::UnboundedSender<SyncEnvelope>,
}

/// 连接中枢：持有监听端口、已连接对端集合、配对码，并协调本地/远端剪贴板流转。
pub struct ConnectionHub {
    #[allow(dead_code)]
    identity: Arc<DeviceIdentity>,
    engine: Arc<SyncEngine>,
    pairing_code: Mutex<String>,
    /// 已建立加密通道的对端（key 为 device_id 或远端地址）
    peers: Mutex<HashMap<String, Peer>>,
    /// 正在发起连接的对端地址集合，避免重复连接
    connecting: Mutex<HashSet<String>>,
    /// 应用句柄（用于广播连接状态事件）
    app: Mutex<Option<AppHandle>>,
}

impl ConnectionHub {
    pub fn new(
        identity: Arc<DeviceIdentity>,
        engine: Arc<SyncEngine>,
        pairing_code: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity,
            engine,
            pairing_code: Mutex::new(pairing_code),
            peers: Mutex::new(HashMap::new()),
            connecting: Mutex::new(HashSet::new()),
            app: Mutex::new(None),
        })
    }

    pub fn set_pairing_code(&self, code: String) {
        *self.pairing_code.lock().unwrap() = code;
    }

    /// 返回当前已建立加密通道的对端 device_id 集合。
    ///
    /// 供前端挂载时主动查询一次，避免错过 `peer-connected` 事件
    /// （该事件可能在 webview 加载、事件监听就绪前就已发出，Tauri 不重放事件）。
    pub fn connected_peer_ids(&self) -> Vec<String> {
        self.peers.lock().unwrap().keys().cloned().collect()
    }

    /// 启动监听器、本地剪贴板广播器，并订阅 mDNS 发现事件以主动连接对端。
    pub async fn start(self: Arc<Self>, app: AppHandle, listen_port: u16) {
        *self.app.lock().unwrap() = Some(app.clone());

        // 1) 本地剪贴板变化广播器：订阅引擎事件，加密转发给所有对端
        {
            let engine = self.engine.clone();
            let hub = self.clone();
            tauri::async_runtime::spawn(async move {
                let mut rx = engine.subscribe();
                while let Ok(ev) = rx.recv().await {
                    let SyncEvent::LocalClipboardChanged { mark, content } = ev;
                    let env = SyncEnvelope { mark, content };
                    let peers = hub.peers.lock().unwrap().clone();
                    for (_id, p) in peers {
                        let _ = p.tx.send(env.clone());
                    }
                }
            });
        }

        // 2) 监听入站 WebSocket 连接（作为 SPAKE2 应答方）
        {
            let hub = self.clone();
            tauri::async_runtime::spawn(async move {
                match TcpListener::bind(("0.0.0.0", listen_port)).await {
                    Ok(listener) => {
                        tracing::info!("ClipSync listening on 0.0.0.0:{}", listen_port);
                        loop {
                            match listener.accept().await {
                                Ok((sock, _)) => {
                                    let ra = sock
                                        .peer_addr()
                                        .map(|a| a.to_string())
                                        .unwrap_or_default();
                                    match tokio_tungstenite::accept_async(sock).await {
                                        Ok(ws) => {
                                            let hub = hub.clone();
                                            tauri::async_runtime::spawn(async move {
                                                let _ = hub
                                                    .run_connection(
                                                        ws,
                                                        Role::Responder,
                                                        ra.clone(),
                                                        "incoming".to_string(),
                                                        ra,
                                                    )
                                                    .await;
                                            });
                                        }
                                        Err(e) => tracing::warn!("ws accept failed: {e}"),
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("accept failed: {e}");
                                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("failed to bind listen port {}: {e}", listen_port)
                    }
                }
            });
        }

        // 3) 订阅 mDNS 发现事件，主动连接对端（作为 SPAKE2 发起方）
        {
            let hub = self.clone();
            app.listen("peer-discovered", move |event| {
                let hub = hub.clone();
                let payload = event.payload().to_string();
                if let Ok(peer) = serde_json::from_str::<DiscoveredPeer>(&payload) {
                    tauri::async_runtime::spawn(async move {
                        hub.connect_to(peer).await;
                    });
                }
            });
        }
    }

    /// 主动连接一个发现的对端，握手成功后保持加密通道；断开后每 5 秒重连。
    pub async fn connect_to(self: Arc<Self>, peer: DiscoveredPeer) {
        let key = format!("{}:{}", peer.addr, peer.port);
        {
            let mut g = self.connecting.lock().unwrap();
            if g.contains(&key) {
                return;
            }
            g.insert(key.clone());
        }
        loop {
            match tokio_tungstenite::connect_async(format!("ws://{}:{}", peer.addr, peer.port)).await
            {
                Ok((ws, _)) => {
                    let id = peer.device_id.clone();
                    let name = peer.device_name.clone();
                    let addr = format!("{}:{}", peer.addr, peer.port);
                    let res = self
                        .clone()
                        .run_connection(ws, Role::Initiator, id, name, addr)
                        .await;
                    if let Err(e) = res {
                        tracing::warn!("peer {} connection ended: {e}", peer.device_name);
                    }
                }
                Err(e) => {
                    tracing::warn!("connect to {} failed: {e}", key);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            tracing::debug!("retrying connection to {}", peer.device_name);
        }
    }

    /// 处理一条已建立的 WebSocket 连接：先 SPAKE2 配对，再进入加密消息循环。
    async fn run_connection<S>(
        self: Arc<Self>,
        mut ws: WebSocketStream<S>,
        role: Role,
        peer_id: String,
        peer_name: String,
        peer_addr: String,
    ) -> anyhow::Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let pw = self.pairing_code.lock().unwrap().clone();
        // SPAKE2 握手：发起方先发、应答方先收
        let key = match role {
            Role::Initiator => {
                let init = start_initiator(&pw);
                send_signal(&mut ws, init.message.clone()).await?;
                let b = recv_signal(&mut ws).await?;
                let shared = init
                    .finish(&b)
                    .map_err(|e| anyhow::anyhow!("spake2 finish: {e}"))?;
                derive_session_key(&shared)?
            }
            Role::Responder => {
                let a = recv_signal(&mut ws).await?;
                let resp = start_responder(&pw);
                send_signal(&mut ws, resp.message.clone()).await?;
                let shared = resp
                    .finish(&a)
                    .map_err(|e| anyhow::anyhow!("spake2 finish: {e}"))?;
                derive_session_key(&shared)?
            }
        };
        tracing::info!("paired with {} ({})", peer_name, peer_addr);

        if let Some(app) = self.app.lock().unwrap().clone() {
            let _ = app.emit(
                "peer-connected",
                serde_json::json!({ "device_id": peer_id, "device_name": peer_name, "addr": peer_addr }),
            );
        }

        // 注册对端，供本地剪贴板转发使用
        let (tx, mut rx) = mpsc::unbounded_channel::<SyncEnvelope>();
        self.peers.lock().unwrap().insert(
            peer_id.clone(),
            Peer {
                tx: tx.clone(),
            },
        );

        // 拆成读写两半，避免 `next()` 与 `send()` 在同一 select! 中争夺 ws 的借用
        let (mut write, mut read) = ws.split();

        // 加密消息循环
        loop {
            tokio::select! {
                outgoing = rx.recv() => {
                    match outgoing {
                        Some(env) => {
                            let pt = bincode::serialize(&env)
                                .map_err(|e| anyhow::anyhow!("serialize envelope: {e}"))?;
                            let nonce: [u8; 12] = rand::random();
                            let ct = encrypt(&key, &nonce, &pt)
                                .map_err(|e| anyhow::anyhow!("encrypt: {e}"))?;
                            let mut payload = Vec::with_capacity(12 + ct.len());
                            payload.extend_from_slice(&nonce);
                            payload.extend_from_slice(&ct);
                            let frame = MessageFrame::new(MessageType::Sync, payload).encode();
                            if write.send(Message::Binary(frame)).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                incoming = read.next() => {
                    match incoming {
                        Some(Ok(Message::Binary(b))) => {
                            if let Ok(f) = MessageFrame::decode(&b) {
                                if f.msg_type == MessageType::Sync {
                                    if f.payload.len() < 12 {
                                        continue;
                                    }
                                    let (nonce, ct) = f.payload.split_at(12);
                                    let mut n = [0u8; 12];
                                    n.copy_from_slice(nonce);
                                    match decrypt(&key, &n, ct) {
                                        Ok(pt) => {
                                            if let Ok(env) =
                                                bincode::deserialize::<SyncEnvelope>(&pt)
                                            {
                                                self.engine.apply_remote(env.mark, env.content).await;
                                            }
                                        }
                                        Err(e) => tracing::warn!("decrypt failed: {e}"),
                                    }
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        _ => {}
                    }
                }
            }
        }

        // 清理
        self.peers.lock().unwrap().remove(&peer_id);
        if let Some(app) = self.app.lock().unwrap().clone() {
            let _ = app.emit("peer-disconnected", &peer_id);
        }
        tracing::info!("connection to {} closed", peer_name);
        Ok(())
    }
}

async fn send_signal<S>(ws: &mut WebSocketStream<S>, msg: Vec<u8>) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    ws.send(Message::Binary(
        MessageFrame::new(MessageType::Signal, msg).encode(),
    ))
    .await
    .map_err(|e| anyhow::anyhow!("ws send: {e}"))
}

async fn recv_signal<S>(ws: &mut WebSocketStream<S>) -> anyhow::Result<Vec<u8>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    while let Some(m) = ws.next().await {
        match m {
            Ok(Message::Binary(b)) => {
                let f = MessageFrame::decode(&b)
                    .map_err(|e| anyhow::anyhow!("decode signal: {e}"))?;
                if f.msg_type == MessageType::Signal {
                    return Ok(f.payload);
                }
            }
            Ok(Message::Close(_)) | Err(_) => {
                return Err(anyhow::anyhow!("connection closed during handshake"));
            }
            _ => {}
        }
    }
    Err(anyhow::anyhow!("connection closed during handshake"))
}

fn derive_session_key(shared: &[u8; 32]) -> anyhow::Result<[u8; 32]> {
    let okm = derive_session_keys(shared, b"clipsync-session-v1")
        .map_err(|e| anyhow::anyhow!("kdf: {e}"))?;
    let (enc, _mac) = split_keys(okm);
    Ok(enc)
}
