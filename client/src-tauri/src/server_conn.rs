//! 跨局域网服务端连接（客户端侧）
//!
//! 负责：
//! - 常连服务端 WS（Token 鉴权 + 心跳），断线指数退避重连。
//! - 接收 `welcome` / `activated` / `deactivated` / `nodes_update` / `relay_text` / `file_notify`。
//! - 维护跨 LAN 已启用节点表与启用态（pending / active）。
//! - 本机剪贴板变化（经 `SyncEngine` 订阅）时，对「跨 lanGroup 且已启用」节点做文字中继路由。
//! - 接收对端 `relay_text` → 解密 → `engine.apply_remote`；接收 `file_notify` → 推送前端「待复制」。

use crate::clipboard::types::{ClipboardContent, FileMeta, SyncMark};
use crate::clipboard::ClipboardProvider;
use crate::crypto::aead::{decrypt, encrypt, KEY_SIZE, NONCE_SIZE};
use crate::crypto::kdf::derive_network_key;
use crate::sync::engine::{SyncEngine, SyncEvent};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;

use crate::AppState;

type WsSink =
    futures_util::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

/// 接入状态：未启用(pending) / 已启用(active) / 未连接(disconnected)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ServerStatus {
    Disconnected,
    Pending,
    Active,
}

impl ServerStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "active" => ServerStatus::Active,
            "pending" => ServerStatus::Pending,
            _ => ServerStatus::Disconnected,
        }
    }
}

/// 服务端下发的跨 LAN 节点信息。
#[derive(Debug, Clone, Serialize)]
pub struct RemoteNode {
    pub device_id: String,
    pub name: String,
    pub lan_group: String,
    pub ext_file_ep: String,
    pub platform: String,
}

/// 前端「待复制（跨 LAN）」条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossLanOffer {
    pub from: String,
    pub from_name: String,
    pub manifest: serde_json::Value,
    pub ext_file_ep: String,
}

// ---- 与服务端一致的消息类型（字段名必须对齐 server/src/models.rs） ----
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientToServer {
    Auth {
        token: String,
        device: DeviceFields,
    },
    Heartbeat,
    RelayText {
        to: String,
        ct: String,
    },
    FileNotify {
        manifest: serde_json::Value,
        ext_file_ep: String,
    },
}

#[derive(Serialize)]
struct DeviceFields {
    id: String,
    name: String,
    lan_group: String,
    ext_file_ep: String,
    platform: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerToClient {
    Welcome {
        status: String,
        network: NetworkFields,
        nodes: Vec<NodeFields>,
    },
    Activated,
    Deactivated,
    NodesUpdate {
        nodes: Vec<NodeFields>,
    },
    RelayText {
        from: String,
        ct: String,
    },
    FileNotify {
        from: String,
        manifest: serde_json::Value,
        ext_file_ep: String,
    },
    Error {
        code: String,
        #[allow(dead_code)]
        msg: String,
    },
}

#[derive(Deserialize)]
struct NetworkFields {
    id: String,
    #[allow(dead_code)]
    name: String,
}

#[derive(Deserialize)]
struct NodeFields {
    device_id: String,
    name: String,
    lan_group: String,
    ext_file_ep: String,
    platform: String,
}

/// 加密载荷：mark + 原始剪贴板内容（接收端据此 apply_remote）。
#[derive(Serialize, Deserialize)]
struct RelayPayload {
    mark: SyncMark,
    content: ClipboardContent,
}

pub struct ServerConn {
    app: AppHandle,
    engine: Arc<SyncEngine>,
    status: AtomicU8, // 0 disconnected, 1 pending, 2 active
    network_id: Mutex<String>,
    network_key: Mutex<Option<[u8; KEY_SIZE]>>,
    nodes: Mutex<Vec<RemoteNode>>,
    our_lan_group: Mutex<String>,
    /// WS 发送通道（消息由连接任务转发）。断开时为 None。
    ws_tx: Mutex<Option<mpsc::UnboundedSender<ClientToServer>>>,
    /// 配置变更（set_config）后唤醒连接循环立即重连。
    reconnect_notify: Notify,
}

impl ServerConn {
    pub fn new(app: AppHandle, engine: Arc<SyncEngine>) -> Arc<Self> {
        Arc::new(Self {
            app,
            engine,
            status: AtomicU8::new(0),
            network_id: Mutex::new(String::new()),
            network_key: Mutex::new(None),
            nodes: Mutex::new(Vec::new()),
            our_lan_group: Mutex::new(String::new()),
            ws_tx: Mutex::new(None),
            reconnect_notify: Notify::new(),
        })
    }

    pub fn status(&self) -> ServerStatus {
        match self.status.load(Ordering::SeqCst) {
            0 => ServerStatus::Disconnected,
            1 => ServerStatus::Pending,
            _ => ServerStatus::Active,
        }
    }

    pub fn nodes(&self) -> Vec<RemoteNode> {
        self.nodes.lock().unwrap().clone()
    }

    /// 启动：连接循环（断线指数退避）+ 引擎事件路由订阅。
    pub fn start(self: &Arc<Self>) {
        // 路由订阅：本机剪贴板变化 → 跨 LAN 中继 / 文件通知
        {
            let conn = self.clone();
            let mut rx = self.engine.subscribe();
            tauri::async_runtime::spawn(async move {
                loop {
                    let ev = match rx.recv().await {
                        Ok(ev) => ev,
                        Err(_) => continue,
                    };
                    match ev {
                        SyncEvent::LocalClipboardChanged { mark, content } => {
                            conn.route_text(&mark, &content);
                        }
                        SyncEvent::LocalFilesCopied { paths } => {
                            conn.route_files(&paths).await;
                        }
                        _ => {}
                    }
                }
            });
        }

        // 连接循环
        let conn = self.clone();
        let app = self.app.clone();
        tauri::async_runtime::spawn(async move {
            let mut backoff: u64 = 2;
            loop {
                let url = app.state::<AppState>().config.lock().server_url.clone();
                if url.is_empty() {
                    conn.set_status(ServerStatus::Disconnected);
                    // 等待「配置已保存」信号或定时轮询，避免空跑。
                    tokio::select! {
                        _ = conn.reconnect_notify.notified() => continue,
                        _ = tokio::time::sleep(Duration::from_secs(10)) => continue,
                    }
                }
                *conn.our_lan_group.lock().unwrap() =
                    infer_lan_group(&app.state::<AppState>().config.lock().lan_group);
                match conn.connect_once(&url).await {
                    Ok(()) => {
                        backoff = 2;
                    }
                    Err(e) => {
                        tracing::warn!("服务端连接失败，{backoff}s 后重试: {e}");
                    }
                }
                conn.set_status(ServerStatus::Disconnected);
                // 断开后等待退避；期间若配置变更被唤醒则立即重连（读取最新配置）。
                tokio::select! {
                    _ = conn.reconnect_notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
                }
                backoff = (backoff * 2).min(60);
            }
        });
    }

    fn set_status(&self, s: ServerStatus) {
        let v = match s {
            ServerStatus::Disconnected => 0u8,
            ServerStatus::Pending => 1u8,
            ServerStatus::Active => 2u8,
        };
        self.status.store(v, Ordering::SeqCst);
        // 注意：前端 server-status 事件按数字 0/1/2 解析，不能发枚举（会序列化成字符串）。
        let _ = self.app.emit("server-status", v);
    }

    /// 配置变更后由 set_config 调用，立即唤醒连接循环重连（无需重启应用）。
    pub fn reconnect(&self) {
        self.reconnect_notify.notify_one();
    }

    /// 建立一条 WS 连接并运行，直到断开返回。
    async fn connect_once(self: &Arc<Self>, url: &str) -> anyhow::Result<()> {
        let (ws, _resp) = tokio_tungstenite::connect_async(url).await?;
        let (mut w_tx, mut w_rx) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<ClientToServer>();

        let cfg = self.app.state::<AppState>().config.lock().clone();
        if cfg.network_token.trim().is_empty() {
            anyhow::bail!("network_token 为空，无法连接服务端");
        }
        let auth = ClientToServer::Auth {
            token: cfg.network_token.clone(),
            device: DeviceFields {
                id: self.engine.device_id().0.clone(),
                name: cfg.device_name.clone(),
                lan_group: self.our_lan_group.lock().unwrap().clone(),
                ext_file_ep: cfg.ext_file_ep.clone(),
                platform: std::env::consts::OS.to_string(),
            },
        };
        send_json(&mut w_tx, &auth).await?;

        *self.ws_tx.lock().unwrap() = Some(tx);

        let mut hb = tokio::time::interval(Duration::from_secs(25));
        loop {
            tokio::select! {
                maybe = rx.recv() => {
                    match maybe {
                        Some(msg) => { if send_json(&mut w_tx, &msg).await.is_err() { break; } }
                        None => break,
                    }
                }
                frame = w_rx.next() => {
                    let msg = match frame {
                        Some(Ok(m)) => m,
                        Some(Err(_)) | None => break,
                    };
                    if !self.handle_server_message(msg, &mut w_tx).await { break; }
                }
                _ = hb.tick() => {
                    if w_tx.send(Message::Text(serde_json::to_string(&ClientToServer::Heartbeat).unwrap().into())).await.is_err() { break; }
                }
            }
        }
        *self.ws_tx.lock().unwrap() = None;
        Ok(())
    }

    /// 收到服务端消息；返回 false 表示连接应断开。
    async fn handle_server_message(self: &Arc<Self>, msg: Message, w_tx: &mut WsSink) -> bool {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => return false,
            Message::Ping(_) => {
                let _ = w_tx.send(Message::Pong(vec![])).await;
                return true;
            }
            _ => return true,
        };
        let parsed: ServerToClient = match serde_json::from_str(&text) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("解析服务端消息失败: {e}");
                return true;
            }
        };
        match parsed {
            ServerToClient::Welcome { status, network, nodes } => {
                *self.network_id.lock().unwrap() = network.id.clone();
                let key = derive_network_key(
                    &self.app.state::<AppState>().config.lock().network_token,
                    &network.id,
                );
                *self.network_key.lock().unwrap() = Some(key);
                self.set_status(ServerStatus::from_str(&status));
                self.update_nodes(nodes);
                true
            }
            ServerToClient::Activated => {
                self.set_status(ServerStatus::Active);
                true
            }
            ServerToClient::Deactivated => {
                self.set_status(ServerStatus::Pending);
                true
            }
            ServerToClient::NodesUpdate { nodes } => {
                self.update_nodes(nodes);
                true
            }
            ServerToClient::RelayText { from, ct } => {
                self.handle_relay_text(&from, &ct).await;
                true
            }
            ServerToClient::FileNotify {
                from,
                manifest,
                ext_file_ep,
            } => {
                self.handle_file_notify(&from, &manifest, &ext_file_ep);
                true
            }
            ServerToClient::Error { code, msg } => {
                tracing::warn!("服务端错误 code={code} msg={msg}");
                if code == "bad_token" {
                    return false;
                }
                true
            }
        }
    }

    fn update_nodes(&self, nodes: Vec<NodeFields>) {
        let mapped: Vec<RemoteNode> = nodes
            .into_iter()
            .map(|n| RemoteNode {
                device_id: n.device_id,
                name: n.name,
                lan_group: n.lan_group,
                ext_file_ep: n.ext_file_ep,
                platform: n.platform,
            })
            .collect();
        *self.nodes.lock().unwrap() = mapped.clone();
        let _ = self.app.emit("server-nodes", mapped);
    }

    /// 接收对端文字中继：解密 → apply_remote。
    async fn handle_relay_text(&self, _from: &str, ct: &str) {
        let key = match self.network_key.lock().unwrap().clone() {
            Some(k) => k,
            None => {
                tracing::warn!("收到 relay_text 但无网络密钥，忽略");
                return;
            }
        };
        let raw = match B64.decode(ct) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("relay_text base64 失败: {e}");
                return;
            }
        };
        if raw.len() < NONCE_SIZE {
            return;
        }
        let (nonce, cipher) = raw.split_at(NONCE_SIZE);
        let nonce_arr: [u8; NONCE_SIZE] = match nonce.try_into() {
            Ok(a) => a,
            Err(_) => {
                tracing::warn!("relay_text nonce 长度异常");
                return;
            }
        };
        let plaintext = match decrypt(&key, &nonce_arr, cipher) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("relay_text 解密失败: {e}");
                return;
            }
        };
        let payload: RelayPayload = match serde_json::from_slice(&plaintext) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("relay_text 载荷解析失败: {e}");
                return;
            }
        };
        self.engine.apply_remote(payload.mark, payload.content).await;
    }

    /// 接收对端文件通知：推前端「待复制（跨 LAN）」。
    fn handle_file_notify(&self, from: &str, manifest: &serde_json::Value, ext_file_ep: &str) {
        let name = self
            .nodes
            .lock()
            .unwrap()
            .iter()
            .find(|n| n.device_id == from)
            .map(|n| n.name.clone())
            .unwrap_or_else(|| from.to_string());
        let offer = CrossLanOffer {
            from: from.to_string(),
            from_name: name,
            manifest: manifest.clone(),
            ext_file_ep: ext_file_ep.to_string(),
        };
        let _ = self.app.emit("cross-lan-file", offer);
    }

    /// 本机文字变化 → 对跨 LAN 已启用节点做中继。
    fn route_text(&self, mark: &SyncMark, content: &ClipboardContent) {
        if self.status() != ServerStatus::Active || self.network_key.lock().unwrap().is_none() {
            return;
        }
        let key = self.network_key.lock().unwrap().clone().unwrap();
        let our_lg = self.our_lan_group.lock().unwrap().clone();
        let nodes = self.nodes.lock().unwrap().clone();
        for n in nodes.iter().filter(|n| lan_differ(&our_lg, &n.lan_group)) {
            let payload = RelayPayload {
                mark: mark.clone(),
                content: content.clone(),
            };
            let plaintext = match serde_json::to_vec(&payload) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("relay_text 序列化失败: {e}");
                    continue;
                }
            };
            let mut rng = rand::thread_rng();
            let mut nonce = [0u8; NONCE_SIZE];
            rng.fill(&mut nonce);
            let cipher = match encrypt(&key, &nonce, &plaintext) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("relay_text 加密失败: {e}");
                    continue;
                }
            };
            let mut blob = Vec::with_capacity(NONCE_SIZE + cipher.len());
            blob.extend_from_slice(&nonce);
            blob.extend_from_slice(&cipher);
            let ct = B64.encode(blob);
            let msg = ClientToServer::RelayText {
                to: n.device_id.clone(),
                ct,
            };
            if let Some(tx)  = self.ws_tx.lock().unwrap().as_ref() {
                let _ = tx.send(msg);
            }
        }
    }

    /// 本机文件拷贝 → 注册到本地文件服务 + 发文件通知给跨 LAN 节点。
    async fn route_files(&self, paths: &[std::path::PathBuf]) {
        if self.status() != ServerStatus::Active {
            return;
        }
        let cfg = self.app.state::<AppState>().config.lock().clone();
        if cfg.ext_file_ep.trim().is_empty() {
            return;
        }
        let our_lg = self.our_lan_group.lock().unwrap().clone();
        let nodes = self.nodes.lock().unwrap().clone();
        if !nodes.iter().any(|n| lan_differ(&our_lg, &n.lan_group)) {
            return;
        }
        let manifest = self
            .app
            .state::<AppState>()
            .file_share
            .register(paths);
        let msg = ClientToServer::FileNotify {
            manifest,
            ext_file_ep: cfg.ext_file_ep.clone(),
        };
        if let Some(tx) = self.ws_tx.lock().unwrap().as_ref() {
            let _ = tx.send(msg);
        }
    }

    /// 跨 LAN 拉取：从对端 ext_file_ep 下载文件并写本机剪贴板。
    pub async fn pull_cross_lan(
        &self,
        ext_file_ep: &str,
        manifest: serde_json::Value,
    ) -> anyhow::Result<()> {
        let files: Vec<FileMeta> = serde_json::from_value(manifest)?;
        let state = self.app.state::<AppState>();
        let sync_dir = {
            let cfg = state.config.lock();
            cfg.sync_dir
                .clone()
                .unwrap_or_else(|| std::env::temp_dir().join("clipsync").to_string_lossy().to_string())
        };
        let mut saved = Vec::new();
        for f in &files {
            let hash = f.hash.clone().unwrap_or_default();
            let url = format!("http://{ext_file_ep}/file/{hash}");
            let bytes = reqwest::get(&url).await?.bytes().await?;
            let dest = std::path::Path::new(&sync_dir).join(&f.file_name);
            if let Some(parent) = dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&dest, &bytes)?;
            saved.push(dest);
        }
        if !saved.is_empty() {
            self.engine
                .clipboard()
                .write_file_paths(&saved)
                .await?;
        }
        Ok(())
    }
}

/// 发送已序列化 JSON 消息。
async fn send_json(w_tx: &mut WsSink, msg: &ClientToServer) -> anyhow::Result<()> {
    let s = serde_json::to_string(msg)?;
    w_tx.send(Message::Text(s.into())).await?;
    Ok(())
}

/// 对比两组 lan_group：空值不参与「跨 LAN」判定（双方都空视为同 LAN）。
fn lan_differ(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        false
    } else {
        a != b
    }
}

/// 推断本机 lan_group：取首个非回环 IPv4 的前 24 位，失败回退空串。
fn infer_lan_group(configured: &str) -> String {
    if !configured.is_empty() {
        return configured.to_string();
    }
    if let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if s.connect("8.8.8.8:80").is_ok() {
            if let Ok(local) = s.local_addr() {
                if let std::net::IpAddr::V4(v4) = local.ip() {
                    let o = v4.octets();
                    return format!("{}.{}.{}", o[0], o[1], o[2]);
                }
            }
        }
    }
    String::new()
}
