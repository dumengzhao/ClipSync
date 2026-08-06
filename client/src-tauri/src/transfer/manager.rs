//! 传输连接管理器
//!
//! 负责把整条同步链路接通：
//! - 在 `listen_port` 上监听入站 WebSocket（作为 SPAKE2 应答方）
//! - 发现对端后，**仅当对端已配对**才自动重连（作为 SPAKE2 发起方）。未配对对端
//!   不会自动连接，必须用户在前端手动发起「配对」——即「强制交互式」配对。
//! - 配对流程（强制交互式）：
//!   * 一方点击「生成配对码」→ 本端生成 6 位随机码并「武装」监听（应答方角色），
//!     该码通过界面展示给用户，由用户线下告知对方。
//!   * 另一方在「局域网发现的设备」中点击「配对」并输入该码 → 本端作为发起方用该码连接。
//!   * 双方先交换 Hello（含身份/公钥），再用该码跑 SPAKE2 握手派生会话密钥，
//!     并以 HMAC 校验确认两端使用同一口令；校验通过后把对端登记为「已配对」。
//! - 已配对对端断线后，本端用之前缓存的口令自动重连（无需再次交互）。
//! - 用 AES-GCM 加密通道传输剪贴板同步包（`SyncEnvelope`）。

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Listener, Manager};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::clipboard::types::{ClipboardContent, DeviceId, SyncMark};
use crate::crypto::aead::{decrypt, encrypt};
use crate::crypto::kdf::{derive_session_keys, split_keys};
use crate::crypto::pake::{generate_pairing_code as gen_code, start_initiator, start_responder};
use crate::device::identity::DeviceIdentity;
use crate::device::registry::{PairedDevice, TrustLevel};
use crate::discovery::DiscoveredPeer;
use crate::sync::engine::{SyncEngine, SyncEvent};
use crate::transfer::websocket::{MessageFrame, MessageType};
use crate::AppState;

/// 配对码「武装」后的有效时长。过期后未完成的配对自动失效，需重新生成。
const PAIRING_ARM_TIMEOUT: Duration = Duration::from_secs(120);

/// 手动配对（发起方）的最大尝试次数，每次间隔 5 秒；与武装有效期接近，避免无限重试。
const MAX_PAIRING_ATTEMPTS: u32 = 24;

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

/// Hello 握手帧：在 SPAKE2 之前交换身份，用于应答方在已知对端身份的前提下选择口令
/// （已配对 → 用缓存口令静默重连；未配对 → 必须处于武装状态且使用武装的配对码）。
#[derive(Clone, Serialize, Deserialize)]
struct HelloPayload {
    device_id: String,
    device_name: String,
    /// 身份公钥（base64），用于生成指纹供用户核对
    public_key: String,
}

/// 连接中枢：持有监听端口、已配对口令缓存、武装中的配对码，并协调本地/远端剪贴板流转。
pub struct ConnectionHub {
    identity: Arc<DeviceIdentity>,
    engine: Arc<SyncEngine>,
    /// 已配对对端的口令缓存（key = device_id），用于断线后自动重连，无需再次交互。
    paired_codes: Mutex<HashMap<String, String>>,
    /// 当前「武装」的配对码（生成配对码后设置），应答方仅在武装状态下接受新配对。
    pending_code: Mutex<Option<(String, Instant)>>,
    /// 已建立加密通道的对端（key 为 device_id）
    peers: Mutex<HashMap<String, Peer>>,
    /// 正在发起连接的对端地址集合，避免重复连接（key = addr:port）
    connecting: Mutex<HashSet<String>>,
    /// 应用句柄（用于广播连接状态事件 / 写注册表）
    app: Mutex<Option<AppHandle>>,
}

impl ConnectionHub {
    pub fn new(identity: Arc<DeviceIdentity>, engine: Arc<SyncEngine>) -> Arc<Self> {
        Arc::new(Self {
            identity,
            engine,
            paired_codes: Mutex::new(HashMap::new()),
            pending_code: Mutex::new(None),
            peers: Mutex::new(HashMap::new()),
            connecting: Mutex::new(HashSet::new()),
            app: Mutex::new(None),
        })
    }

    /// 生成 6 位随机配对码并「武装」监听（应答方角色）。返回该码供前端展示。
    pub fn generate_pairing_code(&self) -> String {
        let code = gen_code();
        *self.pending_code.lock().unwrap() = Some((code.clone(), Instant::now()));
        code
    }

    /// 取消当前武装的配对码（前端「取消配对」时调用）。
    pub fn cancel_pairing(&self) {
        *self.pending_code.lock().unwrap() = None;
    }

    /// 返回当前武装中的配对码（若有且未过期），供前端刷新时恢复展示。
    pub fn pending_pairing_code(&self) -> Option<String> {
        let g = self.pending_code.lock().unwrap();
        match &*g {
            Some((code, t)) if t.elapsed() < PAIRING_ARM_TIMEOUT => Some(code.clone()),
            _ => None,
        }
    }

    /// 若处于武装状态且未过期，返回配对码；过期则自动解除武装并返回 None。
    fn is_armed(&self) -> Option<String> {
        let mut g = self.pending_code.lock().unwrap();
        match &*g {
            Some((code, t)) if t.elapsed() < PAIRING_ARM_TIMEOUT => Some(code.clone()),
            Some(_) => {
                *g = None; // 过期，自动解除武装
                None
            }
            None => None,
        }
    }

    pub fn is_paired(&self, device_id: &str) -> bool {
        self.paired_codes.lock().unwrap().contains_key(device_id)
    }

    /// 返回当前已建立加密通道的对端 device_id 集合（供前端挂载时主动查询一次）。
    pub fn connected_peer_ids(&self) -> Vec<String> {
        self.peers.lock().unwrap().keys().cloned().collect()
    }

    /// 启动监听器、本地剪贴板广播器，并订阅 mDNS 发现事件（仅对已配对对端自动重连）。
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
                                                        "incoming".to_string(),
                                                        "incoming".to_string(),
                                                        ra,
                                                        None,
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

        // 3) 订阅 mDNS 发现事件：仅当对端已配对时才自动重连（强制交互式——
        //    未配对对端必须用户在前端手动发起配对，绝不静默自动连接）。
        {
            let hub = self.clone();
            app.listen("peer-discovered", move |event| {
                let hub = hub.clone();
                let payload = event.payload().to_string();
                if let Ok(peer) = serde_json::from_str::<DiscoveredPeer>(&payload) {
                    if !hub.is_paired(&peer.device_id) {
                        return;
                    }
                    let key = format!("{}:{}", peer.addr, peer.port);
                    let already = hub.peers.lock().unwrap().contains_key(&peer.device_id)
                        || hub.connecting.lock().unwrap().contains(&key);
                    if already {
                        return;
                    }
                    let hub2 = hub.clone();
                    tauri::async_runtime::spawn(async move {
                        hub2.connect_to(peer, None).await;
                    });
                }
            });
        }
    }

    /// 用户在前端手动发起配对：作为发起方，使用用户输入的口令连接对端。
    /// 成功后由 `run_connection` 把口令写入 `paired_codes` 缓存，供后续自动重连。
    /// 若多次尝试仍失败（如配对码错误或对方未生成），则主动结束并通知前端，避免无限重试。
    pub async fn pair_with(self: Arc<Self>, peer: DiscoveredPeer, code: String) {
        let key = format!("{}:{}", peer.addr, peer.port);
        {
            let mut g = self.connecting.lock().unwrap();
            if g.contains(&key) {
                return;
            }
            g.insert(key.clone());
        }
        for attempt in 0..MAX_PAIRING_ATTEMPTS {
            match self
                .clone()
                .connect_once(peer.clone(), Some(code.clone()))
                .await
            {
                // Ok 表示已成功配对并进入加密通道（直到对端断开才返回），直接结束
                Ok(()) => break,
                Err(e) => tracing::warn!("pair attempt {} to {} failed: {e}", attempt + 1, peer.device_name),
            }
            if attempt + 1 < MAX_PAIRING_ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
        // 无论成败都解除连接守卫，允许用户在配对超时后重新发起
        self.connecting.lock().unwrap().remove(&key);
        // 若始终未配对成功，通知前端（成功会写入 paired_codes，is_paired 为真）
        if !self.is_paired(&peer.device_id) {
            if let Some(app) = self.app.lock().unwrap().clone() {
                let _ = app.emit(
                    "pairing-failed",
                    serde_json::json!({
                        "device_id": peer.device_id,
                        "reason": "配对超时，请确认对方已生成相同配对码且仍在有效期内",
                    }),
                );
            }
        }
    }

    /// 单次连接尝试（作为 SPAKE2 发起方）。成功配对并跑完同步循环返回 Ok，握手失败返回 Err。
    async fn connect_once(
        self: Arc<Self>,
        peer: DiscoveredPeer,
        outgoing_code: Option<String>,
    ) -> anyhow::Result<()> {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}:{}", peer.addr, peer.port))
            .await
            .map_err(|e| anyhow::anyhow!("connect failed: {e}"))?;
        let id = peer.device_id.clone();
        let name = peer.device_name.clone();
        let addr = format!("{}:{}", peer.addr, peer.port);
        self.clone()
            .run_connection(ws, Role::Initiator, id, name, addr, outgoing_code)
            .await
    }

    /// 主动连接一个已配对对端（作为 SPAKE2 发起方），断开后每 5 秒自动重连（持久循环）。
    ///
    /// `outgoing_code` 为本次发起携带的配对码；已配对对端重连时传 `None`，
    /// 由 `run_connection` 改用缓存口令。
    pub async fn connect_to(self: Arc<Self>, peer: DiscoveredPeer, outgoing_code: Option<String>) {
        let key = format!("{}:{}", peer.addr, peer.port);
        {
            let mut g = self.connecting.lock().unwrap();
            if g.contains(&key) {
                return;
            }
            g.insert(key.clone());
        }
        loop {
            match self.clone().connect_once(peer.clone(), outgoing_code.clone()).await {
                Ok(()) => {}
                Err(e) => tracing::warn!("peer {} connection ended: {e}", peer.device_name),
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            tracing::debug!("retrying connection to {}", peer.device_name);
        }
    }

    /// 处理一条已建立的 WebSocket 连接：Hello 交换 → 选口令 → SPAKE2 → HMAC 校验 →
    /// 登记已配对 → 进入加密消息循环。
    async fn run_connection<S>(
        self: Arc<Self>,
        mut ws: WebSocketStream<S>,
        role: Role,
        _placeholder_id: String,
        _placeholder_name: String,
        peer_addr: String,
        outgoing_code: Option<String>,
    ) -> anyhow::Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        // 1) Hello 交换：先发自己的身份，再读对方身份
        let my_hello = HelloPayload {
            device_id: self.identity.id.0.clone(),
            device_name: self.identity.name.clone(),
            public_key: base64::engine::general_purpose::STANDARD
                .encode(self.identity.public_key_bytes()),
        };
        send_frame(&mut ws, MessageType::Hello, &serde_json::to_vec(&my_hello)?).await?;
        let (_ht, hpayload) = recv_frame(&mut ws).await?;
        let peer_hello: HelloPayload = serde_json::from_slice(&hpayload)
            .map_err(|e| anyhow::anyhow!("bad hello payload: {e}"))?;
        let peer_id = peer_hello.device_id.clone();
        let peer_name = peer_hello.device_name.clone();

        // 2) 选择口令
        let pw = match role {
            Role::Initiator => {
                // 已配对对端重连 → 用缓存口令；否则必须使用本次发起携带的口令
                self.paired_codes
                    .lock()
                    .unwrap()
                    .get(&peer_id)
                    .cloned()
                    .or(outgoing_code.clone())
                    .ok_or_else(|| anyhow::anyhow!("未提供配对码"))?
            }
            Role::Responder => {
                // 已配对对端重连 → 用缓存口令（静默）；否则必须处于武装状态
                if let Some(code) = self.paired_codes.lock().unwrap().get(&peer_id).cloned() {
                    code
                } else {
                    self.is_armed()
                        .ok_or_else(|| anyhow::anyhow!("对端未发起配对或本端未生成配对码"))?
                }
            }
        };

        // 3) SPAKE2 握手（发起方先发、应答方先收）
        let key = match role {
            Role::Initiator => {
                let init = start_initiator(&pw);
                send_frame(&mut ws, MessageType::Signal, &init.message).await?;
                let (_t, b) = recv_frame(&mut ws).await?;
                let shared = init
                    .finish(&b)
                    .map_err(|e| anyhow::anyhow!("spake2 finish: {e}"))?;
                derive_session_key(&shared)?
            }
            Role::Responder => {
                let (_t, a) = recv_frame(&mut ws).await?;
                let resp = start_responder(&pw);
                send_frame(&mut ws, MessageType::Signal, &resp.message).await?;
                let shared = resp
                    .finish(&a)
                    .map_err(|e| anyhow::anyhow!("spake2 finish: {e}"))?;
                derive_session_key(&shared)?
            }
        };

        // 4) HMAC 校验：确认两端使用同一口令（错误口令会派生不同密钥 → 校验失败）
        let my_tag = hmac_sha256(&key, peer_id.as_bytes());
        send_frame(&mut ws, MessageType::Verify, &my_tag).await?;
        let (_vt, peer_tag) = recv_frame(&mut ws).await?;
        if peer_tag.len() != 32 || peer_tag != my_tag {
            if let Some(app) = self.app.lock().unwrap().clone() {
                let _ = app.emit(
                    "pairing-failed",
                    serde_json::json!({ "device_id": peer_id, "reason": "配对码不一致" }),
                );
            }
            anyhow::bail!("配对校验失败（配对码不一致）");
        }

        tracing::info!("paired with {} ({})", peer_name, peer_addr);

        // 5) 登记为已配对：写注册表 + 缓存口令 + 通知前端
        if let Some(app) = self.app.lock().unwrap().clone() {
            let pk = base64::engine::general_purpose::STANDARD
                .decode(&peer_hello.public_key)
                .map_err(|e| anyhow::anyhow!("bad peer public key: {e}"))?;
            let fingerprint = sha256_hex(&pk).get(..32).unwrap_or("").to_string();
            app.state::<AppState>().registry.lock().add(PairedDevice {
                device_id: DeviceId(peer_id.clone()),
                device_name: peer_name.clone(),
                fingerprint: fingerprint.clone(),
                trust: TrustLevel::Verified,
                last_seen: now_secs(),
            });
            self.paired_codes
                .lock()
                .unwrap()
                .insert(peer_id.clone(), pw.clone());
            let _ = app.emit(
                "peer-paired",
                serde_json::json!({
                    "id": peer_id,
                    "name": peer_name,
                    "fingerprint": fingerprint,
                    "trusted": true,
                    "last_seen": now_secs(),
                }),
            );
            let _ = app.emit(
                "peer-connected",
                serde_json::json!({
                    "device_id": peer_id,
                    "device_name": peer_name,
                    "addr": peer_addr,
                }),
            );
            // 应答方配对成功后解除武装，使该配对码仅用一次
            if let Role::Responder = role {
                self.cancel_pairing();
            }
        }

        // 6) 注册对端，进入加密同步循环
        let (tx, mut rx) = mpsc::unbounded_channel::<SyncEnvelope>();
        self.peers.lock().unwrap().insert(
            peer_id.clone(),
            Peer {
                tx: tx.clone(),
            },
        );

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

async fn send_frame<S>(
    ws: &mut WebSocketStream<S>,
    msg_type: MessageType,
    payload: &[u8],
) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    ws.send(Message::Binary(
        MessageFrame::new(msg_type, payload.to_vec()).encode(),
    ))
    .await
    .map_err(|e| anyhow::anyhow!("ws send: {e}"))
}

/// 读取下一个二进制帧，跳过控制/非二进制帧；连接关闭或出错时返回 Err。
async fn recv_frame<S>(ws: &mut WebSocketStream<S>) -> anyhow::Result<(MessageType, Vec<u8>)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    while let Some(m) = ws.next().await {
        match m {
            Ok(Message::Binary(b)) => {
                let f = MessageFrame::decode(&b)
                    .map_err(|e| anyhow::anyhow!("decode frame: {e}"))?;
                return Ok((f.msg_type, f.payload));
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

type HmacSha256 = Hmac<Sha256>;

fn hmac_sha256(key: &[u8; 32], data: &[u8]) -> Vec<u8> {
    let mut mac =
        HmacSha256::new_from_slice(key).expect("hmac accepts 32-byte key");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    let h = Sha256::digest(data);
    h.iter().map(|b| format!("{:02x}", b)).collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
