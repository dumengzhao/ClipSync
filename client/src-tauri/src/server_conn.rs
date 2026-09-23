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
use crate::device::hardware::hardware_id;
use crate::sync::engine::{SyncEngine, SyncEvent};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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
    /// 到达时间（unix 毫秒）：前端「最新在上」排序用（与局域网待拉取的
    /// `PendingOffer.received_at` 同一语义，两者要合并成一张列表展示）。
    pub received_at: u64,
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
    /// 心跳回执：值本身无需处理——它的作用在 connect_once 的读侧活性检测里兑现
    /// （收到任何入帧都会刷新 last_rx）。
    HeartbeatAck,
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
    /// 跨 LAN 拉取对应的原始文件通知（key = pull_id）：**用户取消时据此定位并删除**
    /// 「待复制」清单里的那一条（取消 = 彻底删除，不留任何痕迹；见 tauri_cmd::pull_cross_lan
    /// 的取消分支）。存的是身份（from + ext_file_ep + manifest），用于精确匹配。
    cross_pull_origin: Mutex<HashMap<String, CrossLanOffer>>,
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
/// 读侧活性判定窗口：超过此时长未收到服务端任何帧即视为链路假死。
/// 心跳周期 25s + 服务端回执，90s ≈ 3 个周期余量，正常链路不会误伤。
const READ_DEADLINE: Duration = Duration::from_secs(90);

/// 规范化「对外文件地址」（`ext_file_ep`）：省略端口时补默认端口。
///
/// 合法写法是 `IPv4[:port]`（也允许域名），端口可省——但**省略时的含义必须在所有
/// 使用点一致**。此前 `probe_ext_file_ep`（设置页「测试并保存」）按 `:{listen_port}`
/// 拼 URL 并据此放行保存，而跨 LAN 拉取直接拼 `http://{ep}` 落到 **80 端口**：
/// 于是「探测通过才能保存」的地址，保存后拉取必然失败（两个使用点各写一份逻辑导致
/// 分叉）。抽成一个函数后不可能再分叉。
/// 校验「对外文件地址」是否是可接受的形式：`host[:port]`，其中 host 为 IPv4/IPv6
/// 字面量或合法域名，端口 1..=65535。
///
/// 拒绝一切多余成分（scheme / 路径 / 查询 / userinfo / 空白 / 控制字符）：
/// 该值会来自**远端**（对端通告、中继下发）并被直接拼成 HTTP 请求地址，
/// 宽松解析等于让远端选择本机去请求谁（SSRF 面）与请求路径形态。
pub(crate) fn ext_file_ep_is_valid(ep: &str) -> bool {
    let ep = ep.trim();
    if ep.is_empty() {
        return false;
    }
    if ep.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    // scheme / 路径 / 查询 / 片段 / userinfo 一律不允许
    if ep.contains("://")
        || ep.contains('/')
        || ep.contains('?')
        || ep.contains('#')
        || ep.contains('@')
    {
        return false;
    }
    match ep.parse::<std::net::SocketAddr>() {
        // 端口 0 必须拒绝：它是「未指定/任意端口」的哨兵值，拼成 `http://host:0`
        // 不可用；而 `SocketAddr` 解析本身允许 `:0`，若在此直接放行，就等于
        // 「IP 字面量 + 0 端口」绕过下方 Err 分支里的端口校验（域名形式:0 反而被拦）。
        Ok(sa) => sa.port() != 0,
        Err(_) => {
            // host[:port] 形式：host 必须是合法域名或 IP；给了端口就要在 1..=65535
            let (host, port) = match ep.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (ep, None),
            };
            if host.is_empty() || host.len() > 253 {
                return false;
            }
            let host_ok = host.parse::<std::net::IpAddr>().is_ok()
                || host.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                });
            if !host_ok {
                return false;
            }
            match port {
                None => true,
                Some(p) => p.parse::<u16>().map(|v| v != 0).unwrap_or(false),
            }
        }
    }
}

pub(crate) fn normalize_ext_file_ep(ep: &str, default_port: u16) -> String {
    let ep = ep.trim();
    if ep.is_empty() || ep.contains(':') {
        return ep.to_string();
    }
    format!("{ep}:{default_port}")
}

/// 把 reqwest 的发送错误翻译成「哪条地址 + 什么原因 + 该怎么办」。
///
/// 用户实测（2026-09-22）界面上只有 reqwest 原文
/// `error sending request for url (http://203.0.113.7:20071/file/…)`，对排查毫无帮助。
fn explain_send_err(base: &str, e: &reqwest::Error) -> String {
    let ep = base.trim_start_matches("http://").trim_end_matches('/');
    let kind = if e.is_timeout() {
        "连接超时（对方未响应：可能防火墙拦截、设备已不在该地址，或该地址只是运营商出口 IP）"
    } else if e.is_connect() {
        "连接被拒绝或不可达"
    } else {
        "请求发送失败"
    };
    format!("对端文件地址 {ep} {kind}。跨 LAN 拉取必须直连对端 20071（中继只转发通知、不转发文件本体）；请确认对方设备已放行该端口、地址填写正确，两台机器同网段时应优先走局域网直连")
}

/// 对端返回非 2xx 时的可读解释（否则前端只能看到「HTTP 404」这种无信息量的话）。
fn explain_http_status(s: reqwest::StatusCode) -> &'static str {
    match s.as_u16() {
        401 | 403 => "对端拒绝了请求：网络密钥不一致，或该文件已不在对端的共享清单里",
        404 => "对端已不再共享该文件（设备重启或剪贴板已被覆盖，可让对方重新复制一次）",
        _ => "对端返回了非 2xx 状态",
    }
}

/// 汇总 manifest（`Vec<FileMeta>` 的 JSON）的文件数 / 总大小 / 文件名列表，供日志使用。
///
/// 对端可控，返回的字符串进日志前由调用方过 `log_safe`（见 `handle_file_notify`）。
/// 文件名最多列 8 个，超出以「…共 N 个」收尾，避免超长日志行。
fn summarize_manifest(manifest: &serde_json::Value) -> (usize, u64, String) {
    let Some(arr) = manifest.as_array() else {
        return (0, 0, "<manifest 不是数组>".to_string());
    };
    let mut total: u64 = 0;
    let mut names: Vec<String> = Vec::new();
    for it in arr {
        total = total.saturating_add(it.get("file_size").and_then(|v| v.as_u64()).unwrap_or(0));
        if names.len() < 8 {
            if let Some(n) = it.get("file_name").and_then(|v| v.as_str()) {
                names.push(n.to_string());
            }
        }
    }
    let tail = if arr.len() > names.len() {
        format!("…共 {} 个", arr.len())
    } else {
        String::new()
    };
    (arr.len(), total, format!("{}{}", names.join("、"), tail))
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
            cross_pull_origin: Mutex::new(HashMap::new()),
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
                            conn.route_text(&mark, &content).await;
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
                            // 曾入网后断开：与「已连接」的 INFO 配对留痕，否则一次干净断线
                            // 在日志里毫无痕迹（失败 WARN 只在重连失败后才出现）
                            tracing::info!("服务端连接已断开，将自动重连");
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
        // 读侧活性检测：服务端对每条心跳回 HeartbeatAck（见 ws.rs），健康链路最坏
        // ~25s 必有入帧。连续 90s 无任何入帧 = 链路单向假死（写侧 TCP/代理链静默
        // 丢弃，本地发送永远"成功"），必须主动断开重连——否则客户端会永久显示
        // 已连接而服务端早已按空闲超时关连接（2026-09-17 实际发生，经 Clash 代理链）。
        let mut last_rx = tokio::time::Instant::now();
        loop {
            let rx_deadline = last_rx + READ_DEADLINE;
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
                    last_rx = tokio::time::Instant::now();
                    if !self.handle_server_message(msg, &mut w_tx, &mut outcome).await { break; }
                }
                _ = hb.tick() => {
                    if w_tx.send(Message::Text(serde_json::to_string(&ClientToServer::Heartbeat).unwrap())).await.is_err() { break; }
                }
                _ = tokio::time::sleep_until(rx_deadline) => {
                    tracing::warn!("服务端 {READ_DEADLINE:?} 内无任何入帧（链路疑似单向假死），主动断开重连");
                    break;
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
                *self.app.state::<AppState>().network_key.lock().unwrap() = Some(key);
                // 成功入网即清除拉黑标记（管理员恢复设备 / 误报后自愈），下次循环不再走拉黑重试分支
                self.removed.store(false, Ordering::SeqCst);
                self.set_status(ServerStatus::from_str(&status));
                // 成功必须留 INFO：失败路径每周期刷 WARN，而成功若静默，「日志突然安静」
                // 无法区分「已连接」与「重连循环卡死」（2026-09-16 排查时只能靠 netstat 反查）
                tracing::info!(
                    "服务端已连接（中继模式）：网络 {}，在线设备 {} 台",
                    crate::obs::logging::log_safe(&network.id),
                    nodes.len()
                );
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
            ServerToClient::HeartbeatAck => {
                // 无需处理：作用在 connect_once 的读侧活性检测（last_rx）里兑现
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
        // 观测：这条路径此前一行日志都没有 —— 2026-09-22 排查「弹窗里有错误 / 条目数与预期不符」
        // 时完全无从下手（用户只复制了一次却出现多条通知，无法判定是发送侧还是接收侧）。
        // 记录来源、文件数、总大小、地址与文件名，足以复现协议层的行为。
        let (file_count, total_bytes, names) = summarize_manifest(manifest);
        tracing::info!(
            "收到跨 LAN 文件通知：来自 {}（{}），{} 个文件 / {} 字节，地址 {}，文件名 [{}]",
            crate::obs::logging::log_safe(&name),
            from,
            file_count,
            total_bytes,
            ext_file_ep,
            crate::obs::logging::log_safe(&names)
        );
        let offer = CrossLanOffer {
            from: from.to_string(),
            from_name: name,
            manifest: manifest.clone(),
            ext_file_ep: ext_file_ep.to_string(),
            received_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        };
        let _ = self.app.emit("cross-lan-file", offer);
        // 跨 LAN 文件到达同样要弹出「待拉取小窗」。
        // 此前这里只 emit 给主窗口，小窗(PullToast)既没监听 cross-lan-file、
        // 也没被通知 show，于是表现为「主窗口能看到待拉取文件，小窗却不弹」。
        crate::transfer::manager::ConnectionHub::show_pull_toast(&self.app);
    }

    /// 本机文字变化 → 对跨 LAN 已启用节点做中继。
    async fn route_text(&self, mark: &SyncMark, content: &ClipboardContent) {
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
            // rng（ThreadRng，!Send）必须在 await 前析构，故置于独立作用域
            let ct = {
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
                B64.encode(blob)
            };
            let msg = ClientToServer::RelayText {
                to: n.device_id.clone(),
                ct,
            };
            // Sender 先克隆再 await：不能拿着 MutexGuard 跨 await（future 会变 !Send）
            let tx_clone = self.ws_tx.lock().as_ref().cloned();
            if let Some(tx) = tx_clone {
                crate::outbox::send_payload(
                    &tx,
                    "剪贴板中继内容",
                    "中继连接",
                    msg,
                    crate::outbox::PAYLOAD_SEND_TIMEOUT,
                )
                .await;
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
        let tx_clone = self.ws_tx.lock().as_ref().cloned();
        if let Some(tx) = tx_clone {
            crate::outbox::send_payload(
                &tx,
                "文件通知",
                "中继连接",
                msg,
                crate::outbox::PAYLOAD_SEND_TIMEOUT,
            )
            .await;
        }
    }

    /// 请求取消指定 pull_id 的跨 LAN 拉取：返回 true 表示已登记（有拉取在等它生效）。
    /// 取消标记是「尽力而为」的集合：登记后即使该拉取已结束也无副作用，由
    /// 下载循环收尾时清除；条目极小，无需淘汰策略。
    /// 取出（并移除）跨 LAN 拉取对应的原始文件通知：供「取消后从清单里删除该条目」定位使用。
    pub fn take_cross_pull_origin(&self, pull_id: &str) -> Option<CrossLanOffer> {
        self.cross_pull_origin.lock().remove(pull_id)
    }

    /// 丢弃跨 LAN 拉取对应的原始通知（拉取成功/失败等无需再定位删除的结局）。
    pub fn drop_cross_pull_origin(&self, pull_id: &str) {
        self.cross_pull_origin.lock().remove(pull_id);
    }

    pub fn cancel_cross_pull(&self, pull_id: &str) -> bool {
        self.cross_pull_cancel.lock().insert(pull_id.to_string())
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
        // 先留一份：manifest 下面会被 from_value 移走，而「取消时删除该条目」（取消路径）还要用
        let manifest_for_restore = manifest.clone();
        let files: Vec<FileMeta> = serde_json::from_value(manifest)?;
        // 存一份原始通知，供「取消后从清单里删除该条目」定位使用（见 take_cross_pull_origin）
        self.cross_pull_origin.lock().insert(
            pull_id.to_string(),
            CrossLanOffer {
                from: from.to_string(),
                from_name: {
                    let n = self
                        .nodes
                        .lock()
                        .iter()
                        .find(|n| n.device_id == from)
                        .map(|n| n.name.clone());
                    n.unwrap_or_else(|| from.to_string())
                },
                manifest: manifest_for_restore,
                ext_file_ep: ext_file_ep.to_string(),
                received_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            },
        );
        let state = self.app.state::<AppState>();
        // 落盘根目录：与 P2P 路径**共用同一套解析规则**（sync_dir 配置优先 → 系统下载目录）。
        // 早先这里单独回退到 temp_dir()/clipsync，且 sync_dir 只对 P2P 生效 ——
        // 同一台设备按传输路径不同会落到两处，配置项对跨 LAN 也不起作用。
        let sync_dir = {
            let configured = state.config.lock().sync_dir.clone();
            crate::transfer::paths::resolve_sync_dir(Some(&self.app), configured)
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
        //   省略端口时补默认端口，规则与设置页探测共用 normalize_ext_file_ep。
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
        // 对端（或中继）提供的地址**不可信**：形态非法一律丢弃该候选，
        // 绝不拿它去发请求（宽松解析 = 让远端选择本机请求谁与请求路径形态）。
        let ep = {
            let raw = ext_file_ep.trim();
            if raw.is_empty() {
                String::new()
            } else if ext_file_ep_is_valid(raw) {
                normalize_ext_file_ep(raw, state.config.lock().listen_port)
            } else {
                tracing::warn!(
                    "忽略形态非法的对外文件地址：{}",
                    crate::obs::logging::log_safe(raw)
                );
                String::new()
            }
        };
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
        // 候选链可变：命中路由的候选**被提到首位、失败候选被剔除**，因此后续文件
        // 第一跳即命中，不必重复等 3s connect_timeout；但保留「逐个候选都试」，
        // 使任一文件在中途路由失效时都能回退（详见下方选路处注释）。
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
                let key = *self.network_key.lock();
                key.map(|k| crate::crypto::file_auth::auth_token(&k, &hash))
                    .unwrap_or_default()
            };
            // 依次尝试各候选，第一个返回 2xx 的胜出并把其提到首位：
            // - 误连到别家内网同 IP 的陌生设备会 404 / 解密失败，自然落到下一个候选；
            // - **必须逐个候选都试**（不能「首文件成功后锁死 routes[0] 单次尝试」）：
            //   锁死版在后续文件上遇到中途失效的路由（内网掉线 / 代理重启）会直接
            //   终止整个 pull，而首个文件明明有回退能力 —— 同一批文件里行为不一致；
            //   且锁死版不校验状态码，非 2xx 的响应体会被当作文件内容下载，最终只报
            //   「解密失败」，掩盖真实原因。
            // - 性能：命中路由已在 index 0 且失败候选已被剔除，后续文件第一跳即命中，
            //   不会重复等待 connect_timeout（这正是原先想做「锁死」的动机，已由
            //   「提前 + 剔除」达成）。
            let (resp, route) = {
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
                        Ok(r) => {
                            last_err = Some(anyhow::anyhow!(
                                "对端返回 HTTP {}：{}",
                                r.status(),
                                explain_http_status(r.status())
                            ))
                        }
                        Err(e) => {
                            // reqwest 原文（`error sending request for url (http://…)`）用户实测
                            // 完全看不懂（2026-09-22）：既不知哪条地址、也不知该怎么办。
                            // 原文降到 DEBUG 留档，面向前端的是「地址 + 原因 + 怎么办」。
                            tracing::debug!(
                                "跨 LAN 拉取 {pull_id} 候选地址 {base} 发送失败（原文）: {e}"
                            );
                            last_err = Some(anyhow::anyhow!("{}", explain_send_err(base, &e)))
                        }
                    }
                    i += 1;
                }
                match chosen {
                    Some(c) => c,
                    None => {
                        return Err(
                            last_err.unwrap_or_else(|| anyhow::anyhow!("所有拉取地址均不可达"))
                        );
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

/// 对比两组 lan_group：**是否需要走跨 LAN 中继**（true = 需要）。
///
/// 任一侧「不可信」（空串 / 格式异常 / 虚拟网段）时返回 true —— 语义是「不知道是否同网，
/// 保守按需要中继处理」。注意这与旧实现相反：旧实现把空串当作「同组」返回 false，
/// 结果是本机分组推断失败时 route_files 的中继通知与 relay_text 对所有对端都静默不发。
fn lan_differ(a: &str, b: &str) -> bool {
    // 任一侧不可信 → 按「可能需要中继」处理（true）：原实现把空串判为「同组」，
    // 于是在本机分组推断失败时，route_files 的中继通知与 relay_text 会**整体静默不发**
    // （对所有对端都是 false）。保守发多一份由内容去重/服务端同组过滤兜底。
    if group_is_unreliable(a) || group_is_unreliable(b) {
        true
    } else {
        a != b
    }
}

/// lan_group 是否**不可信**（空串 / 格式异常 / 虚拟网段）。
///
/// 虚拟段见 `is_virtual_or_reserved`：装 Clash/TUN 的机器会把 TUN 网卡地址当分组，
/// 使两个真实网络里的设备被判成同组（通知漏投）、或同 LAN 设备被判成不同组（重复）。
/// 与服务端 `state::group_is_unreliable` 保持同一语义。
fn group_is_unreliable(g: &str) -> bool {
    let mut it = g.split('.');
    match (it.next(), it.next(), it.next()) {
        (Some(a), Some(b), Some(c)) => match (a.parse::<u8>(), b.parse::<u8>(), c.parse::<u8>()) {
            (Ok(a), Ok(b), Ok(c)) => is_virtual_or_reserved(std::net::Ipv4Addr::new(a, b, c, 0)),
            _ => true,
        },
        _ => true,
    }
}

/// 已知的「虚拟网卡 / 保留段」IPv4 —— 不能用来判断局域网分组。
///
/// - `198.18.0.0/15`：IETF 基准测试保留段，Clash 等代理的 TUN fake-ip 常用；
/// - `100.64.0.0/10`：运营商级 NAT（Tailscale 等 VPN 也用）。
fn is_virtual_or_reserved(ip: std::net::Ipv4Addr) -> bool {
    let o = ip.octets();
    let in_198_18 = o[0] == 198 && (o[1] == 18 || o[1] == 19);
    let in_100_64 = o[0] == 100 && (64..=127).contains(&o[1]);
    in_198_18 || in_100_64
}

/// 从候选 IPv4 里选出代表「本机真实局域网」的那个（纯函数，便于单测）。
///
/// 规则：剔除回环/链路本地/虚拟段后，优先私有地址（10/8、172.16/12、192.168/16），
/// 其次任意剩余地址；排序保证同一台机器每次结果一致（接口枚举顺序不保证稳定）。
fn pick_lan_ipv4(candidates: &[std::net::Ipv4Addr]) -> Option<std::net::Ipv4Addr> {
    let mut ok: Vec<std::net::Ipv4Addr> = candidates
        .iter()
        .copied()
        .filter(|ip| !ip.is_loopback() && !ip.is_link_local() && !is_virtual_or_reserved(*ip))
        .collect();
    ok.sort();
    ok.iter()
        .find(|ip| ip.is_private())
        .copied()
        .or_else(|| ok.first().copied())
}

/// 推断本机 lan_group（取所选 IPv4 的前 24 位；无法判定时回退空串）。
///
/// 顺序：① 配置里显式指定则直接用；② 枚举网卡、剔除回环/链路本地/虚拟网段，
/// 优先私有地址（见 `pick_lan_ipv4`）；③ 兜底用「默认路由出口地址」。
/// pub(crate)：transfer/manager 也需要同源算法做「只看局域网」过滤，避免两处拷贝。
pub(crate) fn infer_lan_group(configured: &str) -> String {
    if !configured.is_empty() {
        return configured.to_string();
    }
    // 先枚举真实网卡并排除虚拟/保留段。**不能只用「连 8.8.8.8 看默认路由出口地址」**：
    // 装了 Clash/TUN 的机器该地址是 TUN 网卡的 198.18.x（基准测试保留段），于是
    // 同一真实局域网的两台机器可能被判成不同组、而分处两个真实网络的机器被判成同组
    // ——跨 LAN 文件通知因此投错或漏投（2026-09-17 实测：Mac mini 与一台公网机器都报 198.18.0）。
    let candidates: Vec<std::net::Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => Some(v4.ip),
            if_addrs::IfAddr::V6(_) => None,
        })
        .collect();
    if let Some(ip) = pick_lan_ipv4(&candidates) {
        let o = ip.octets();
        return format!("{}.{}.{}", o[0], o[1], o[2]);
    }
    // 兜底：保留原「默认路由出口」逻辑（可能拿到虚拟地址，但比空串保守——
    // 空串会让服务端按「不确定」处理，宁可重复显示也不漏投）
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

#[cfg(test)]
mod tests {
    use super::{normalize_ext_file_ep, pick_lan_ipv4};
    /// 分组不可信判定：空/格式异常/虚拟网段都算不可信（保守投递），真实私有组才可信。
    #[test]
    fn unreliable_lan_group_detection() {
        use super::group_is_unreliable;
        assert!(group_is_unreliable(""));
        assert!(group_is_unreliable("10.0")); // 格式异常
        assert!(group_is_unreliable("abc.def.ghi"));
        assert!(group_is_unreliable("198.18.0")); // Clash TUN fake-ip
        assert!(group_is_unreliable("100.64.0")); // CGNAT / VPN
        assert!(!group_is_unreliable("10.0.0"));
        assert!(!group_is_unreliable("192.168.1"));
    }

    /// 不可信分组 → 视为「可能跨 LAN」（保守发中继）；两边都可信才按异同判断。
    #[test]
    fn lan_differ_is_conservative_for_unreliable_groups() {
        use super::lan_differ;
        assert!(!lan_differ("10.0.0", "10.0.0"));
        assert!(lan_differ("10.0.0", "10.0.1"));
        assert!(lan_differ("", "10.0.0"));
        assert!(lan_differ("198.18.0", "198.18.0"));
    }

    /// 虚拟/保留网段不能当局域网分组依据：Clash TUN（198.18.x）与 CGNAT/VPN（100.64+）
    /// 会污染分组，导致同 LAN 判成不同组、不同网络判成同组（2026-09-17 实测）。
    #[test]
    fn virtual_ranges_are_ignored_when_picking_lan_ip() {
        use std::net::Ipv4Addr;
        // 只有 TUN 地址 → 不选它（返回 None，交由兜底逻辑）
        assert_eq!(pick_lan_ipv4(&[Ipv4Addr::new(198, 18, 114, 130)]), None);
        assert_eq!(pick_lan_ipv4(&[Ipv4Addr::new(100, 64, 0, 5)]), None);
        // 有真实私有地址时优先私有，忽略 TUN 与公网
        assert_eq!(
            pick_lan_ipv4(&[
                Ipv4Addr::new(198, 18, 0, 1),
                Ipv4Addr::new(103, 40, 14, 14),
                Ipv4Addr::new(10, 0, 0, 146),
            ]),
            Some(Ipv4Addr::new(10, 0, 0, 146))
        );
        // 只有公网（无虚拟段）时按排序取稳定结果
        assert_eq!(
            pick_lan_ipv4(&[Ipv4Addr::new(103, 40, 14, 14), Ipv4Addr::new(101, 1, 1, 1)]),
            Some(Ipv4Addr::new(101, 1, 1, 1))
        );
        // 回环/链路本地剔除
        assert_eq!(
            pick_lan_ipv4(&[Ipv4Addr::new(127, 0, 0, 1), Ipv4Addr::new(169, 254, 1, 1)]),
            None
        );
    }

    /// 省略端口时必须补默认端口：这是「设置页探测通过 → 保存后跨 LAN 拉取可用」
    /// 不变式的一半（另一半是拉取端用同一个函数）。回归点：拉取端曾直接拼
    /// `http://{ep}`，无端口 ep 会打到 80 端口，与探测结论矛盾。
    #[test]
    fn ep_without_port_gets_default() {
        assert_eq!(
            normalize_ext_file_ep("192.0.2.10", 20071),
            "192.0.2.10:20071"
        );
        assert_eq!(
            normalize_ext_file_ep("  relay.example.com  ", 20071),
            "relay.example.com:20071"
        );
    }

    /// 显式端口（对端走内网穿透时 ≠ 20071）必须原样保留，绝不能被本机端口覆盖。
    #[test]
    fn ep_with_port_kept_verbatim() {
        assert_eq!(
            normalize_ext_file_ep("192.0.2.10:30080", 20071),
            "192.0.2.10:30080"
        );
        assert_eq!(normalize_ext_file_ep("1.2.3.4:80", 20071), "1.2.3.4:80");
    }

    /// 严格校验：远端可控的值不允许携带 scheme/路径/userinfo/空白，端口范围也要合法。
    #[test]
    fn ep_validation_rejects_injection_forms() {
        use super::ext_file_ep_is_valid as ok;
        // 合法：IPv4 / IPv6 / 域名，可省端口
        assert!(ok("192.0.2.10"));
        assert!(ok("192.0.2.10:20071"));
        assert!(ok("relay.example.com:443"));
        assert!(ok("[2001:db8::1]:20071"));
        // 裸 IPv6（无方括号）必须拒绝：与 host:port 语法无法区分（rsplit_once(':') 会把
        // "::1" 末段当端口），实现按 host[:port] 解析必然失败。合法形式必须带方括号。
        assert!(!ok("2001:db8::1"));
        // 非法：scheme / 路径 / userinfo / 空白 / 空host / 端口越界 / 控制字符
        assert!(!ok(""));
        assert!(!ok("http://evil.example"));
        assert!(!ok("192.0.2.10/file/x"));
        assert!(!ok("user@evil.example"));
        assert!(!ok("192.0.2.10:0"));
        assert!(!ok("192.0.2.10:70000"));
        assert!(!ok("evil example.com"));
        assert!(!ok("evil
.example.com"));
        assert!(!ok(":20071"));
    }

    /// 空串 = 未配置（跨 LAN 文件不可拉取），保持空串，不能变成 ":20071"。
    #[test]
    fn empty_ep_stays_empty() {
        assert_eq!(normalize_ext_file_ep("", 20071), "");
        assert_eq!(normalize_ext_file_ep("   ", 20071), "");
    }
}
