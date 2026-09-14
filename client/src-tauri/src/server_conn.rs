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
use std::collections::HashSet;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;

use crate::AppState;

type WsSink = futures_util::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

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
    hardware_id: String,
    /// 操作系统版本号（如 macOS 14.5 / Windows 11 Pro），由客户端上报供管理后台展示
    os_version: String,
}

/// 跨平台硬件唯一标识（用于服务端区分同一台物理机器）。
/// 优先级：macOS IOPlatformUUID / Windows MachineGuid / Linux /etc/machine-id；
/// 均失败时返回空串，由调用方以 device_id（持久化 UUID）兜底。
fn hardware_id() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sh")
            .arg("-c")
            .arg("ioreg -rd1 -c IOPlatformExpertDevice 2>/dev/null | grep IOPlatformUUID")
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout);
            // 形如：  "IOPlatformUUID" = "XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX"
            let parts: Vec<&str> = s.split('"').collect();
            if parts.len() >= 4 {
                let h = parts[3].trim().to_string();
                if !h.is_empty() {
                    return h;
                }
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        // reg 是控制台程序：不设 CREATE_NO_WINDOW 会闪出命令窗口
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if let Ok(out) = std::process::Command::new("reg")
            .args([
                "query",
                "HKLM\\SOFTWARE\\Microsoft\\Cryptography",
                "/v",
                "MachineGuid",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(h) = s.split_whitespace().last() {
                let h = h.trim().to_string();
                if !h.is_empty() {
                    return h;
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/etc/machine-id") {
            let s = s.trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    String::new()
}

/// 跨平台获取操作系统版本号（如 macOS 14.5 / Windows 11 Pro / Ubuntu 22.04.3 LTS）。
/// 失败时返回空串，由服务端以「未知」占位。
fn os_version() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        // reg 是控制台程序：不设 CREATE_NO_WINDOW 会闪出命令窗口
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if let Ok(out) = std::process::Command::new("reg")
            .args([
                "query",
                "HKLM\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion",
                "/v",
                "ProductName",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout);
            // 注册表输出形如：`    ProductName    REG_SZ    Windows 11 Pro`
            if let Some(idx) = s.find("REG_SZ") {
                let v = s[idx + "REG_SZ".len()..].trim().to_string();
                // 去掉前缀 "Windows "，避免与前端 osLabel 重复
                let v = v.strip_prefix("Windows ").unwrap_or(&v).to_string();
                if !v.is_empty() {
                    return v;
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/etc/os-release") {
            for line in s.lines() {
                if let Some(v) = line.strip_prefix("PRETTY_NAME=") {
                    let v = v.trim_matches('"').to_string();
                    if !v.is_empty() {
                        return v;
                    }
                }
            }
        }
    }
    String::new()
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
    /// 服务端已将该设备从网络移除（拉黑）：停止重连，提示需重新配对
    Removed,
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

/// 一次 WS 连接的结束方式，供连接循环区分「网络故障」与「鉴权被拒」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectOutcome {
    /// 曾成功入网（收到 Welcome）后断开：视为正常连接周期，复位退避。
    Authed,
    /// 鉴权被拒（bad_token）：属于配置错误而非网络故障，不复位退避，
    /// 以长退避周期重试（服务端数据丢失等极端场景可自愈）。
    AuthRejected,
    /// 未经鉴权即断开（连接失败 / 网络中断 / Close 帧）。
    Disconnected,
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
    ///
    /// **有界**：与 P2P 出站同理，服务端消费不过来时不能让队列无限堆积
    /// （剪贴板中继与文件通知都可能持续产生）。满时 `try_send` 丢弃并记日志。
    ws_tx: Mutex<Option<mpsc::Sender<ClientToServer>>>,
    /// 配置变更（set_config）后唤醒连接循环立即重连。
    reconnect_notify: Notify,
    /// 被服务端移除（拉黑）标记：置位后仍周期性重试以便自愈，管理员后台恢复设备后下次连接即清除。
    removed: AtomicBool,
    /// 鉴权失败（bad_token）提示去重标记：仅首次向前端发 server-auth-rejected，
    /// 避免 60s 重试周期反复弹提示。入网成功或用户重存配置（reconnect）时复位。
    auth_fail_notified: AtomicBool,
    /// 跨 LAN 拉取取消标记（key = pull_id）：`cancel_pull_cross_lan` 置位，
    /// 下载循环在每个分片边界检查，命中即中止本次拉取。
    cross_pull_cancel: Mutex<HashSet<String>>,
    /// 硬件 ID / OS 版本缓存：reg 查询是控制台子进程（虽然已加 CREATE_NO_WINDOW
    /// 不闪窗），也不该在每次重连时重复执行——启动后缓存一次即可。
    cached_hw_id: Mutex<String>,
    cached_os_ver: Mutex<String>,
}

/// 跨 LAN 单文件体积上限。
///
/// 跨 LAN 拉取目前是「整文件读入内存 → 原地解密 → 写盘」（原地解密已把峰值从 2× 降到 ≈1×，
/// 见 `crypto::aead::decrypt_in_place`）。真正做流式需要把服务端的**整体加密**改成
/// 分块加密协议（两端同步变更），暂不引入；这里先用上限保护内存，超限提示走局域网 P2P
/// ——P2P 路径本身就是流式的，不受此限。
const MAX_CROSS_LAN_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// 中继（客户端 → 服务端）出站队列容量。
///
/// 与 P2P 出站同理取小值：队列满即说明服务端消费不过来，此时丢弃新的中继消息
/// 好过无界堆积（剪贴板中继与文件通知都会持续产生）。
const RELAY_QUEUE_CAPACITY: usize = 64;

/// 记录一次中继出站消息被丢弃的原因（Full = 队列满丢弃 / Closed = 连接已关闭）。
fn log_relay_dropped(what: &str, e: &mpsc::error::TrySendError<ClientToServer>) {
    match e {
        mpsc::error::TrySendError::Full(_) => tracing::warn!(
            "中继出站队列已满（容量 {RELAY_QUEUE_CAPACITY}），{what} 被丢弃（服务端消费过慢）"
        ),
        mpsc::error::TrySendError::Closed(_) => {
            tracing::debug!("中继连接已关闭，{what} 未发送")
        }
    }
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
            removed: AtomicBool::new(false),
            auth_fail_notified: AtomicBool::new(false),
            cross_pull_cancel: Mutex::new(HashSet::new()),
            cached_hw_id: Mutex::new(String::new()),
            cached_os_ver: Mutex::new(String::new()),
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
        self.nodes.lock().clone()
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
                        // broadcast 语义要分开处理：
                        // Lagged = 本端消费慢、丢了若干条 → 跳过继续；
                        // Closed = 所有发送端已 drop，再 continue 就是 100% CPU 空转，必须退出。
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
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

        // 连接循环启动前，后台线程预热硬件 ID / OS 版本（reg 子进程较慢，不占连接路径）
        self.prewarm_device_info();

        // 连接循环
        let conn = self.clone();
        let app = self.app.clone();
        tauri::async_runtime::spawn(async move {
            let mut backoff: u64 = 2;
            loop {
                // 被服务端移除（拉黑）：仍周期性重试，以便管理员在后台恢复设备后客户端能自愈；
                // 退避 30s 避免频繁打扰服务端。用户手动重存配置会经 reconnect() 立即唤醒重试。
                if conn.removed.load(Ordering::SeqCst) {
                    conn.set_status(ServerStatus::Disconnected);
                    tokio::select! {
                        _ = conn.reconnect_notify.notified() => { backoff = 2; }
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                    }
                    // 落到下方尝试连接：仍被拉黑会再次收到 Removed 回到此处；
                    // 已恢复则收到 Welcome 并清除 removed 标记（见 handle_server_message）。
                }
                let url = app.state::<AppState>().config.lock().server_url.clone();
                if url.is_empty() {
                    conn.set_status(ServerStatus::Disconnected);
                    // 等待「配置已保存」信号或定时轮询，避免空跑。
                    tokio::select! {
                        _ = conn.reconnect_notify.notified() => continue,
                        _ = tokio::time::sleep(Duration::from_secs(10)) => continue,
                    }
                }
                *conn.our_lan_group.lock() =
                    infer_lan_group(&app.state::<AppState>().config.lock().lan_group);
                match conn.connect_once(&url).await {
                    Ok(outcome) => match outcome {
                        ConnectOutcome::Authed => {
                            backoff = 2;
                            // 本轮曾成功入网：复位鉴权失败提示标记，之后再失败可重新提示
                            conn.auth_fail_notified.store(false, Ordering::SeqCst);
                        }
                        ConnectOutcome::AuthRejected => {
                            // token 失效是配置错误而非网络抖动：不复位退避，改用长退避周期
                            // 重试（服务端数据丢失等极端场景可自愈）；用户重存配置会被
                            // reconnect_notify 立即唤醒。提示只发一次，避免每个重试周期都弹。
                            if !conn.auth_fail_notified.swap(true, Ordering::SeqCst) {
                                let _ = app.emit("server-auth-rejected", ());
                            }
                            backoff = backoff.max(60);
                        }
                        ConnectOutcome::Disconnected => {
                            tracing::warn!("服务端连接断开，{backoff}s 后重试");
                        }
                    },
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
    /// 同时清除「已被移除」标记，使被拉黑后管理员恢复的设备可重新尝试入网。
    ///
    /// 注意：已建立连接时，连接循环正阻塞在 `connect_once` 内读服务端消息，并不在
    /// `reconnect_notify` 上等待，仅靠 `notify_one` 会被丢弃。因此这里先丢弃 WS 发送端，
    /// 使 `connect_once` 的 mpsc `rx.recv()` 返回 `None` 而退出，连接循环随即用最新配置
    /// 重连（Auth 携带更新后的 ext_file_ep 等），服务端 handle_auth 据此刷新节点信息。
    pub fn reconnect(&self) {
        self.removed.store(false, Ordering::SeqCst);
        // 复位鉴权失败提示去重：用户刚改过配置，重试后若仍被拒应再次收到提示
        self.auth_fail_notified.store(false, Ordering::SeqCst);
        {
            let mut tx = self.ws_tx.lock();
            *tx = None;
        }
        self.reconnect_notify.notify_one();
    }

    /// 后台预热硬件 ID / OS 版本缓存（reg 子进程同步阻塞，不能放在连接关键路径上）。
    /// 在 start() 里用独立线程调用一次；connect_once 只读缓存。
    pub fn prewarm_device_info(self: &Arc<Self>) {
        let conn = self.clone();
        std::thread::spawn(move || {
            let hw = hardware_id();
            let os = os_version();
            let mut hw_guard = conn.cached_hw_id.lock();
            if hw_guard.is_empty() {
                *hw_guard = hw;
            }
            drop(hw_guard);
            *conn.cached_os_ver.lock() = os;
        });
    }

    /// 建立一条 WS 连接并运行，直到断开返回。返回本次连接的结束方式：
    /// 网络层错误为 `Err`，连接建立后按是否入网 / 是否被拒给出 `ConnectOutcome`。
    async fn connect_once(self: &Arc<Self>, url: &str) -> anyhow::Result<ConnectOutcome> {
        let (ws, _resp) = tokio_tungstenite::connect_async(url).await?;
        let (mut w_tx, mut w_rx) = ws.split();
        let (tx, mut rx) = mpsc::channel::<ClientToServer>(RELAY_QUEUE_CAPACITY);

        let cfg = self.app.state::<AppState>().config.lock().clone();
        if cfg.network_token.trim().is_empty() {
            anyhow::bail!("network_token 为空，无法连接服务端");
        }
        let mut hw = self.cached_hw_id.lock().clone();
        if hw.is_empty() {
            // 预热尚未完成（极快重连时可能撞上）：用 device_id 兜底，不阻塞等待子进程
            hw = self.engine.device_id().0.clone();
        }
        let os_ver = self.cached_os_ver.lock().clone();
        let auth = ClientToServer::Auth {
            token: cfg.network_token.clone(),
            device: DeviceFields {
                id: self.engine.device_id().0.clone(),
                name: cfg.device_name.clone(),
                lan_group: self.our_lan_group.lock().clone(),
                ext_file_ep: cfg.ext_file_ep.clone(),
                platform: std::env::consts::OS.to_string(),
                hardware_id: hw,
                os_version: os_ver,
            },
        };
        send_json(&mut w_tx, &auth).await?;

        *self.ws_tx.lock() = Some(tx);

        let mut outcome = ConnectOutcome::Disconnected;
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
                    if !self.handle_server_message(msg, &mut w_tx, &mut outcome).await { break; }
                }
                _ = hb.tick() => {
                    if w_tx.send(Message::Text(serde_json::to_string(&ClientToServer::Heartbeat).unwrap())).await.is_err() { break; }
                }
            }
        }
        *self.ws_tx.lock() = None;
        Ok(outcome)
    }

    /// 收到服务端消息；返回 false 表示连接应断开。
    /// `outcome` 随关键消息更新：Welcome → Authed，bad_token → AuthRejected；
    /// 连接循环据此区分「曾入网后断开」（复位退避）与「鉴权被拒」（长退避 + 提示）。
    async fn handle_server_message(
        self: &Arc<Self>,
        msg: Message,
        w_tx: &mut WsSink,
        outcome: &mut ConnectOutcome,
    ) -> bool {
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
            ServerToClient::Welcome {
                status,
                network,
                nodes,
            } => {
                *outcome = ConnectOutcome::Authed;
                *self.network_id.lock() = network.id.clone();
                let key = derive_network_key(
                    &self.app.state::<AppState>().config.lock().network_token,
                    &network.id,
                );
                *self.network_key.lock() = Some(key);
                // 同步到 AppState，供内嵌 HTTP 文件服务加密 / 拉取端解密复用同一个网络密钥
                // 注意：AppState.network_key 是 std::sync::Mutex（与 ServerConn 自身那个
                // parking_lot::Mutex 类型不同），所以这里仍需 .unwrap()
                *self
                    .app
                    .state::<AppState>()
                    .network_key
                    .lock()
                    .unwrap() = Some(key);
                // 成功入网即清除拉黑标记（管理员恢复设备 / 误报后自愈），下次循环不再走拉黑重试分支
                self.removed.store(false, Ordering::SeqCst);
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
            ServerToClient::Removed => {
                // 被服务端移除（拉黑）：停止重连，提示用户需重新配对
                self.removed.store(true, Ordering::SeqCst);
                self.set_status(ServerStatus::Disconnected);
                let _ = self.app.emit("server-removed", ());
                false
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
                    *outcome = ConnectOutcome::AuthRejected;
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
        *self.nodes.lock() = mapped.clone();
        let _ = self.app.emit("server-nodes", mapped);
    }

    /// 接收对端文字中继：解密 → apply_remote。
    async fn handle_relay_text(&self, _from: &str, ct: &str) {
        let key = match *self.network_key.lock() {
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
        self.engine
            .apply_remote(payload.mark, payload.content)
            .await;
    }

    /// 接收对端文件通知：推前端「待复制（跨 LAN）」。
    fn handle_file_notify(&self, from: &str, manifest: &serde_json::Value, ext_file_ep: &str) {
        let name = self
            .nodes
            .lock()
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
        // 跨 LAN 文件到达同样要弹出「待拉取小窗」。
        // 此前这里只 emit 给主窗口，小窗(PullToast)既没监听 cross-lan-file、
        // 也没被通知 show，于是表现为「主窗口能看到待拉取文件，小窗却不弹」。
        crate::transfer::manager::ConnectionHub::show_pull_toast(&self.app);
    }

    /// 本机文字变化 → 对跨 LAN 已启用节点做中继。
    fn route_text(&self, mark: &SyncMark, content: &ClipboardContent) {
        if self.status() != ServerStatus::Active {
            return;
        }
        // 一次加锁取出密钥快照：早些时候写成「先 is_none() 判断、再 unwrap()」，
        // 两次独立加锁之间存在检查-使用竞态（中途被清空会 panic）。
        let Some(key) = *self.network_key.lock() else {
            tracing::debug!("relay_text：网络密钥未就绪，跳过该次中继");
            return;
        };
        let our_lg = self.our_lan_group.lock().clone();
        let nodes = self.nodes.lock().clone();
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
            if let Some(tx) = self.ws_tx.lock().as_ref() {
                if let Err(e) = tx.try_send(msg) {
                    log_relay_dropped("剪贴板中继内容", &e);
                }
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
        let our_lg = self.our_lan_group.lock().clone();
        let nodes = self.nodes.lock().clone();
        if !nodes.iter().any(|n| lan_differ(&our_lg, &n.lan_group)) {
            return;
        }
        let manifest = self.app.state::<AppState>().file_share.register(paths);
        let msg = ClientToServer::FileNotify {
            manifest,
            ext_file_ep: cfg.ext_file_ep.clone(),
        };
        if let Some(tx) = self.ws_tx.lock().as_ref() {
            if let Err(e) = tx.try_send(msg) {
                log_relay_dropped("文件通知", &e);
            }
        }
    }

    /// 请求取消指定 pull_id 的跨 LAN 拉取：返回 true 表示已登记（有拉取在等它生效）。
    /// 取消标记是「尽力而为」的集合：登记后即使该拉取已结束也无副作用，由
    /// 下载循环收尾时清除；条目极小，无需淘汰策略。
    pub fn cancel_cross_pull(&self, pull_id: &str) -> bool {
        self.cross_pull_cancel
            .lock()
            .insert(pull_id.to_string())
    }

    /// 跨 LAN 拉取：按发送方 device_id 优先走内网直连，回退 ext_file_ep。
    /// `pull_id` 是前端「待拉取条目」的唯一 id（不含 `local:` 前缀），用于把
    /// 进度事件(`file-pull-progress`/`file-pull-complete`)精准投递给对应条目，
    /// 让小窗进度条能实时更新（历史 bug：跨 LAN 路径从不发进度事件，进度条卡 0%）。
    ///
    /// 选路规则：查本机 mDNS 发现表（`discovered`）——mDNS 只在局域网内生效，
    /// 能发现即证明发送方确实在本内网且地址可达，直接用表内 addr + SRV 真实端口；
    /// 表内无（真跨 LAN）→ ext_file_ep 兜底。实际选中的路由随
    /// progress/complete 事件的 `route` 字段上报（"lan" / "wan"），前端可见。
    pub async fn pull_cross_lan(
        &self,
        pull_id: &str,
        from: &str,
        ext_file_ep: &str,
        manifest: serde_json::Value,
    ) -> anyhow::Result<()> {
        let files: Vec<FileMeta> = serde_json::from_value(manifest)?;
        let state = self.app.state::<AppState>();
        let sync_dir = {
            let cfg = state.config.lock();
            cfg.sync_dir.clone().unwrap_or_else(|| {
                std::env::temp_dir()
                    .join("clipsync")
                    .to_string_lossy()
                    .to_string()
            })
        };
        // 落盘目录：`sync_dir/<设备名>/`（与 P2P 路径一致：平铺、同名覆盖）。
        // 设备名优先取已配对 registry 的显示名，取不到（未配对/已解配）才回退 device_id；
        // 一律经 safe_segment 净化，杜绝名字中的路径成分。
        let device_dir = {
            let name = state
                .registry
                .lock()
                .list()
                .into_iter()
                .find(|d| d.device_id.0 == from)
                .map(|d| d.device_name)
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| from.to_string());
            crate::transfer::paths::safe_segment(&name)
        };
        let root = std::path::Path::new(&sync_dir).join(device_dir);
        if let Err(e) = std::fs::create_dir_all(&root) {
            tracing::warn!("创建跨 LAN 接收目录失败（落盘可能失败）: {e}");
        }
        // 候选链：① 内网直连（本机 mDNS 发现表 SRV 真实端口）→ ② 对端 ext_file_ep
        //   拉取端直接按对端通告的「完整地址（IPv4[:port]）」直连，不读取本机 listen_port
        //   ——对端若走内网穿透，代理端口很可能 ≠ 20071，本机端口作兜底会拼错。
        let mut routes: Vec<(String, &'static str)> = Vec::new();
        {
            let lan = state
                .discovered
                .lock()
                .values()
                .find(|p| p.device_id == from)
                .filter(|p| !p.addr.is_empty())
                .map(|p| (format!("http://{}:{}", p.addr, p.port), "lan"));
            if let Some(r) = lan {
                routes.push(r);
            }
        }
        let ep = ext_file_ep.trim();
        if !ep.is_empty() {
            let wan = (format!("http://{ep}"), "wan");
            // 避免与候选①完全重复（内网可达时不绕外网）
            if !routes.iter().any(|(u, _)| *u == wan.0) {
                routes.push(wan);
            }
        }
        if routes.is_empty() {
            return Err(anyhow::anyhow!(
                "对端不在本机局域网发现表中，且未配置对外文件地址（ext_file_ep），无法拉取"
            ));
        }
        // 清掉此前针对本 pull_id 残留的取消标记（用户取消后立刻重新点拉取的场景）
        self.cross_pull_cancel.lock().remove(pull_id);
        // 连接超时：内网地址不可达时能快速回退到下一个候选，不至于长时间挂起
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(3))
            .build()?;
        let app = &self.app;
        // 总大小（明文，与前端 itemSize 对齐）用于进度百分比
        let total_plain: u64 = files.iter().map(|f| f.file_size).sum();
        let mut done_plain: u64 = 0u64;
        let mut saved = Vec::new();
        let mut route_used: &'static str = "";
        let mut start_emitted = false;
        // 候选链可变：首个文件试探成功后剔除失败项，后续文件直接用命中路由
        // ——避免每个文件重复等 3s connect_timeout，大文件夹首文件后秒级完成。
        let mut routes: Vec<(String, &'static str)> = routes;
        for f in &files {
            // 取消检查点 1：每个文件开始前。命中即清标记并整体中止。
            if self.cross_pull_cancel.lock().remove(pull_id) {
                // 清理已写入的临时文件，不留半截垃圾
                for p in &saved {
                    let _ = std::fs::remove_file(p);
                }
                tracing::info!("跨 LAN 拉取 {pull_id} 已被用户取消");
                return Err(anyhow::anyhow!("__CANCELLED__"));
            }
            // 体积上限：跨 LAN 路径是「整文件读入内存 → 原地解密 → 写盘」（峰值 ≈1× 文件大小），
            // 超大文件仍会吃光内存。局域网 P2P 路径是流式的（边读边发 / 边收边写），不受此限。
            if f.file_size > MAX_CROSS_LAN_FILE_BYTES {
                return Err(anyhow::anyhow!(
                    "跨 LAN 单文件上限 {} MiB：{} 为 {} MiB，请改用局域网直连传输",
                    MAX_CROSS_LAN_FILE_BYTES / (1024 * 1024),
                    f.file_name,
                    f.file_size / (1024 * 1024)
                ));
            }
            let hash = f.hash.clone().unwrap_or_default();
            // 请求方凭证：证明本端持有同一网络密钥（对端 file_server 会校验，
            // 未持密钥的同网段主机无法拉取共享文件）。密钥未就绪则为空串，
            // 对端会以 401 拒绝——这正是期望行为（无密钥不该能取文件）。
            let auth = {
                let key = *self
                    .network_key
                    .lock();
                key.map(|k| crate::crypto::file_auth::auth_token(&k, &hash))
                    .unwrap_or_default()
            };
            // 试探：首个文件按候选链试，成功路由记录 route_used 并从 routes 里
            // 剔除失败项；后续文件直接走 routes[0]（确定下来的路由），不再轮询。
            let (resp, route) = if !route_used.is_empty() {
                let (base, tag) = &routes[0];
                (
                    client
                        .get(format!("{base}/file/{hash}"))
                        .header("X-Clipsync-Auth", auth.clone())
                        .send()
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?,
                    *tag,
                )
            } else {
                // 依次尝试各路由，第一个返回 2xx 的胜出（误连到别家内网同 IP 的
                // 陌生设备会 404 / 解密失败，自然落到下一个候选，不会拿到错数据）
                let mut chosen: Option<(reqwest::Response, &'static str)> = None;
                let mut last_err: Option<anyhow::Error> = None;
                let mut i = 0usize;
                while i < routes.len() {
                    let (base, tag) = &routes[i];
                    match client
                        .get(format!("{base}/file/{hash}"))
                        .header("X-Clipsync-Auth", auth.clone())
                        .send()
                        .await
                    {
                        Ok(r) if r.status().is_success() => {
                            chosen = Some((r, *tag));
                            // 把命中路由提到首位、剔除之前的失败项
                            let hit = routes.swap_remove(i);
                            routes.insert(0, hit);
                            break;
                        }
                        Ok(r) => last_err = Some(anyhow::anyhow!("HTTP {}", r.status())),
                        Err(e) => last_err = Some(anyhow::anyhow!("{e}")),
                    }
                    i += 1;
                }
                match chosen {
                    Some(c) => c,
                    None => {
                        return Err(last_err
                            .unwrap_or_else(|| anyhow::anyhow!("所有拉取地址均不可达")));
                    }
                }
            };
            route_used = route;
            if !start_emitted {
                start_emitted = true;
                // 路由确定后再发 start（内网直连通常瞬时；ext_file_ep 兜底时最多
                // 等一个连接超时），前端路由徽标从进度一开始就可见
                let _ = app.emit(
                    "file-pull-start",
                    serde_json::json!({ "transfer_id": pull_id, "route": route }),
                );
            }
            let enc_len = resp.content_length().unwrap_or(0);
            // 流式下载到临时文件，边下边上报进度——大文件也能看到中间进度，
            // 不再「等很久一直 0%」。
            // 目标一律落在设备名目录下、只用净化后的文件名（不用对端的 relative_path）
            let dest = root.join(crate::transfer::paths::safe_segment(&f.file_name));
            let tmp = dest.with_extension(format!("{}.clipsync.tmp", std::process::id()));
            {
                use tokio::io::AsyncWriteExt;
                let mut tmpf = tokio::fs::File::create(&tmp)
                    .await
                    .map_err(|e| anyhow::anyhow!("创建临时文件失败: {e}"))?;
                let mut stream = resp.bytes_stream();
                let mut received: u64 = 0;
                let mut last_pct: u32 = 0;
                let mut last_at = std::time::Instant::now();
                while let Some(chunk) = stream.next().await {
                    // 取消检查点 2：下载中每个分片边界，大文件也能即时终止
                    if self.cross_pull_cancel.lock().contains(pull_id) {
                        drop(tmpf);
                        let _ = std::fs::remove_file(&tmp);
                        self.cross_pull_cancel.lock().remove(pull_id);
                        for p in &saved {
                            let _ = std::fs::remove_file(p);
                        }
                        tracing::info!("跨 LAN 拉取 {pull_id} 已被用户取消（下载中）");
                        return Err(anyhow::anyhow!("__CANCELLED__"));
                    }
                    let chunk = chunk.map_err(|e| anyhow::anyhow!("下载失败: {e}"))?;
                    tmpf.write_all(&chunk)
                        .await
                        .map_err(|e| anyhow::anyhow!("写临时文件失败: {e}"))?;
                    received += chunk.len() as u64;
                    // 按「密文已下比例」估算当前文件明文进度（密文 = nonce12 + 明文）；
                    // enc_len 未知（0）或乘法溢出时按整个文件已完成处理
                    let file_done = received
                        .checked_mul(f.file_size)
                        .and_then(|n| n.checked_div(enc_len))
                        .unwrap_or(f.file_size);
                    let overall = done_plain + file_done;
                    let pct = (overall * 100 / total_plain.max(1)) as u32;
                    let now = std::time::Instant::now();
                    if pct.saturating_sub(last_pct) >= 5
                        || now.duration_since(last_at) >= std::time::Duration::from_millis(200)
                    {
                        last_pct = pct;
                        last_at = now;
                        let _ = app.emit(
                            "file-pull-progress",
                            serde_json::json!({
                                "transfer_id": pull_id,
                                "received": overall,
                                "total": total_plain,
                                "percent": pct,
                                "route": route,
                            }),
                        );
                    }
                }
                tmpf.flush().await.ok();
            }
            // 读取下载内容并解密写盘。
            //
            // 解密失败 / 密钥未就绪一律**报错并清理**：早先的实现把这两种情况直接写盘
            // （注释写着「按明文写盘」，实际写出的是密文），用户拿到打不开的文件却看到
            // 「已保存」——这比明确失败更糟。宁可失败得清楚。
            let mut raw = tokio::fs::read(&tmp)
                .await
                .map_err(|e| anyhow::anyhow!("读取临时文件失败: {e}"))?;
            // 失败清理：删临时文件 + 已落盘的前序文件，不留半截结果
            let cleanup = |tmp: &std::path::Path, saved: &[PathBuf]| {
                let _ = std::fs::remove_file(tmp);
                for p in saved {
                    let _ = std::fs::remove_file(p);
                }
            };
            // AppState.network_key 是 std::sync::Mutex → 需要 unwrap
            let key = *state.network_key.lock().unwrap();
            let Some(k) = key else {
                cleanup(&tmp, &saved);
                return Err(anyhow::anyhow!(
                    "网络密钥未就绪，无法解密跨 LAN 文件（请确认已连接服务端）"
                ));
            };
            if raw.len() < NONCE_SIZE + crate::crypto::aead::in_place_overhead() {
                cleanup(&tmp, &saved);
                return Err(anyhow::anyhow!("跨 LAN 文件格式异常（长度不足）"));
            }
            let nonce_arr: [u8; NONCE_SIZE] = {
                let mut n = [0u8; NONCE_SIZE];
                n.copy_from_slice(&raw[..NONCE_SIZE]);
                n
            };
            // 原地剥掉 nonce 前缀（drain 是内存内搬移，不额外分配）
            raw.drain(..NONCE_SIZE);
            // 原地解密：密文缓冲区直接变明文，内存峰值从 2× 降到 ≈1×
            if let Err(e) = crate::crypto::aead::decrypt_in_place(&k, &nonce_arr, &mut raw) {
                cleanup(&tmp, &saved);
                return Err(anyhow::anyhow!(
                    "跨 LAN 文件解密失败（密钥不匹配或数据损坏）: {e}"
                ));
            }
            tokio::fs::remove_file(&tmp).await.ok();
            if let Some(parent) = dest.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return Err(anyhow::anyhow!("创建目标目录失败: {e}"));
                }
            }
            if let Err(e) = tokio::fs::write(&dest, &raw).await {
                return Err(anyhow::anyhow!("写入文件失败: {e}"));
            }
            saved.push(dest.clone());
            done_plain += f.file_size;
            // 文件边界补报一次精确百分比
            let pct = (done_plain * 100 / total_plain.max(1)) as u32;
            let _ = app.emit(
                "file-pull-progress",
                serde_json::json!({
                    "transfer_id": pull_id,
                    "received": done_plain,
                    "total": total_plain,
                    "percent": pct,
                    "route": route,
                }),
            );
        }
        if !saved.is_empty() {
            // 回声抑制：拉取完成后写本机剪贴板，若被本机监听误判为新的文件拷贝，
            // 会经 relay 把 FileNotify 回环广播回发送端，导致「对端复制的文件又出现
            // 在对端待拉取列表」。与 P2P 拉取路径(pull_files)一致——登记路径哈希，
            // 使本地监听判定为回声而丢弃，彻底切断回环。
            self.engine.suppress_next_file_offer(&saved);
            self.engine.clipboard().write_file_paths(&saved).await?;
        }
        // 收尾：显式上报 100%，再发 complete（若失败由调用方补发 ok:false）
        let _ = app.emit(
            "file-pull-progress",
            serde_json::json!({
                "transfer_id": pull_id,
                "received": total_plain,
                "total": total_plain,
                "percent": 100u32,
                "route": route_used,
            }),
        );
        let _ = app.emit(
            "file-pull-complete",
            serde_json::json!({
                "transfer_id": pull_id,
                "device_name": "",
                "target_dir": sync_dir,
                "route": route_used,
                "file_count": saved.len(),
                "files": files
                    .iter()
                    .map(|f| serde_json::json!({
                        "name": f.file_name,
                        "size": f.file_size,
                        "is_dir": f.is_dir,
                    }))
                    .collect::<Vec<_>>(),
                "pulled_at": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                "ok": true,
            }),
        );
        tracing::info!(
            "跨 LAN 拉取 {pull_id} 完成（路由：{}），共写入 {done_plain} 字节",
            if route_used == "lan" {
                "内网直连"
            } else {
                "外网地址"
            }
        );
        Ok(())
    }
}

/// 发送已序列化 JSON 消息。
async fn send_json(w_tx: &mut WsSink, msg: &ClientToServer) -> anyhow::Result<()> {
    let s = serde_json::to_string(msg)?;
    w_tx.send(Message::Text(s)).await?;
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
/// pub(crate)：transfer/manager 也需要同源算法做「只看局域网」过滤，避免两处拷贝。
pub(crate) fn infer_lan_group(configured: &str) -> String {
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
