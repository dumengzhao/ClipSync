//! 传输连接管理器
//!
//! 负责把整条同步链路接通：
//! - 在 `listen_port` 上监听入站 WebSocket（作为 SPAKE2 应答方）
//! - 发现对端后，**仅当对端已配对**才自动重连（作为 SPAKE2 发起方，用缓存的
//!   link secret 静默握手）。未配对对端不会自动连接，需用户在前端手动发起「配对」。
//! - 配对口令统一取自设置中的「预留配对码」（两端必须相同，不再每次随机生成）：
//!   局域网发现设备点「配对」、手动地址点「配对」、以及手动地址监控的兜底直连，
//!   都以此静态码作为 SPAKE2 口令完成首配对。
//! - 配对流程：一方在「局域网发现的设备」或「手动连接地址」中点击「配对」，
//!   本端作为发起方用静态码连接对端；对端作为应答方也以同一静态码回应。双方先
//!   交换 Hello（含身份/公钥），再跑 SPAKE2 握手派生会话密钥，并以 HMAC 校验确认
//!   两端使用同一口令；校验通过后把对端登记为「已配对」。
//! - 配对成功后双方各自派生同一个高熵 **link secret**（由会话密钥派生，不复用
//!   用户设置的配对码），持久化到系统密钥链。此后重连一律用它当 SPAKE2 口令，
//!   **重启也不需要再配对**。
//! - 一个后台监控任务周期性巡检：凡是「已配对 + 已被发现 + 未连接」的对端，
//!   立即发起重连。不依赖 mDNS 事件时序，因此进程重启、对端重启、网络抖动后
//!   都能自愈。
//! - 用 AES-GCM 加密通道传输剪贴板同步包（`SyncEnvelope`）。

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

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

use crate::clipboard::types::{ClipboardContent, DeviceId, FileMeta, SyncMark};
use crate::clipboard::ClipboardProvider;
use crate::crypto::aead::{decrypt, encrypt};
use crate::crypto::kdf::{derive_session_keys, split_keys};
use crate::crypto::pake::{start_initiator, start_responder};
use crate::device::identity::DeviceIdentity;
use crate::device::registry::{PairedDevice, TrustLevel};
use crate::config::settings::ManualAddress;
use crate::discovery::DiscoveredPeer;
use crate::sync::engine::{SyncEngine, SyncEvent};
use crate::transfer::websocket::{FileChunkResponsePayload, FileFrame, MessageFrame, MessageType};
use crate::AppState;
use std::path::PathBuf;

/// 手动配对（发起方）的最大尝试次数，每次间隔 5 秒。
const MAX_PAIRING_ATTEMPTS: u32 = 24;

/// 口令确认标签的域分隔串，避免该 HMAC 与其它用途的 MAC 混淆。
const VERIFY_CONTEXT: &[u8] = b"clipsync-verify-v1";

/// 长期重连口令（link secret）的派生域分隔串。
const LINK_CONTEXT: &[u8] = b"clipsync-link-v1";

/// 连接监控巡检间隔：每轮检查所有「已配对但未连接」的对端并尝试重连。
const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);

/// 握手阶段可区分的失败原因，供上层决定是否重试。
#[derive(Debug)]
enum HandshakeError {
    /// 两端配对码不一致。相同的码重试多少次都不会成功，必须立即终止。
    CodeMismatch,
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CodeMismatch => write!(f, "配对码不一致"),
        }
    }
}

impl std::error::Error for HandshakeError {}

/// 跨连接传输的剪贴板信封（加密前 / 解密后）
#[derive(Clone, Serialize, Deserialize)]
pub struct SyncEnvelope {
    pub mark: SyncMark,
    pub content: ClipboardContent,
    /// 网格中继消息 id：本机每次剪贴板变化生成一次，跨连接转发时保持不变，用于去重防回环。
    pub msg_id: String,
    /// 剩余跳数：每中继一次 -1，减到 0 停止转发。
    pub ttl: u8,
}

#[derive(Clone, Copy)]
enum Role {
    Initiator,
    Responder,
}

/// 连接序号发生器，用于区分同一对端的新旧连接。
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

/// 经由本连接出站的消息。两类都经会话密钥 AES-GCM 加密后发出：
/// - `Sync`：剪贴板内容信封（文本/图片）
/// - `File`：文件传输帧（清单/拉取请求/分片），见 `FileFrame`
pub(crate) enum Outgoing {
    Sync(SyncEnvelope),
    File(FileFrame),
}

/// 单个已连接对端（用于把本地剪贴板变化 / 文件帧转发给它）
#[derive(Clone)]
struct Peer {
    /// 本条连接的唯一序号。清理时据此判断表中登记的是否仍是自己，
    /// 避免**已过期的旧连接把刚建立的新连接从表里删掉**（那会导致本机只收不发）。
    conn_id: u64,
    tx: mpsc::UnboundedSender<Outgoing>,
}

/// 本端作为「发送方」（拷贝者）时保存的一次传输：仅本端持有本地绝对路径，绝不外传。
#[derive(Clone)]
#[allow(dead_code)]
struct OfferState {
    device_name: String,
    files: Vec<FileMeta>,
    local_paths: Vec<PathBuf>,
}

/// 本端作为「接收方」收到的待拉取清单（由对端 `Offer` 填入，不含本地路径）。
#[derive(Clone)]
struct PendingOffer {
    transfer_id: String,
    device_id: String,
    device_name: String,
    files: Vec<FileMeta>,
    /// 顶层条目名（文件夹名或文件名），用于前端折叠显示
    top_names: Vec<String>,
    /// 顶层是否含目录（文件夹传输），用于前端折叠/隐藏大小
    has_folder: bool,
}

/// 本端正在拉取的传输：写入任务通过 `chunk_tx` 接收分片，最终自动写本机剪贴板。
#[allow(dead_code)]
struct PullState {
    chunk_tx: tokio::sync::mpsc::Sender<Option<FileChunkResponsePayload>>,
    target_dir: PathBuf,
    files: Vec<FileMeta>,
    device_id: String,
    total_bytes: u64,
}

/// Hello 握手帧：在 SPAKE2 之前交换身份，用于应答方在已知对端身份的前提下选择口令
/// （已配对 → 用缓存口令静默重连；未配对 → 必须处于武装状态且使用武装的配对码）。
#[derive(Clone, Serialize, Deserialize)]
struct HelloPayload {
    device_id: String,
    device_name: String,
    /// 身份公钥（base64），用于生成指纹供用户核对
    public_key: String,
    /// 本端 WebSocket 监听端口。对端据此拼出可拨号地址（对端 IP + 此端口），
    /// 使应答方在 mDNS 失效时也能记录 last_addr 兜底重连——应答方从 TCP 连接里
    /// 只能拿到对端的临时源端口，无法直接回拨。
    listen_port: u16,
}

/// 网格中继最大跳数：一条剪贴板变化最多经多少台中间设备中转。
/// 仅兜底切断异常网络下的无限转发，正常网格（数十节点内）远不会触及。
const RELAY_MAX_TTL: u8 = 32;

/// 网格中继去重表：记录最近处理过的剪贴板消息 id，防止连通图中消息无限回环。
/// 有界：环形缓冲 + 集合，最多保留 `CAP` 条，超出后最旧者被挤出（旧 id 无害，仅占位）。
struct RelaySeen {
    ring: std::collections::VecDeque<String>,
    set: std::collections::HashSet<String>,
}
impl RelaySeen {
    fn new() -> Self {
        Self {
            ring: std::collections::VecDeque::with_capacity(256),
            set: std::collections::HashSet::new(),
        }
    }
    /// 返回 true 表示 `id` 已处理过（应丢弃）；false 表示首次见到并已记录。
    fn note(&mut self, id: &str) -> bool {
        const CAP: usize = 8192;
        if self.set.contains(id) {
            return true;
        }
        if self.ring.len() >= CAP {
            if let Some(old) = self.ring.pop_front() {
                self.set.remove(&old);
            }
        }
        self.ring.push_back(id.to_string());
        self.set.insert(id.to_string());
        false
    }
}

/// 连接中枢：持有监听端口、已配对口令缓存、本机配对码，并协调本地/远端剪贴板流转。
pub struct ConnectionHub {
    identity: Arc<DeviceIdentity>,
    engine: Arc<SyncEngine>,
    /// 已配对对端的 **link secret**（key = device_id）：配对成功时由会话密钥派生，
    /// 同时写入密钥链持久化。断线/重启后都用它当 SPAKE2 口令静默重连，无需再次交互。
    paired_codes: Mutex<HashMap<String, String>>,
    /// 本机「配对码」（来自设置，应答方常驻口令）。首配对时由应答方用它当 SPAKE2 口令，
    /// 发起方则使用对方显示的码；两端各自独立、无需预先相同。配置加载/保存时由
    /// `set_pairing_code` 同步。
    pairing_code: Mutex<String>,
    /// 文件夹文件数上限：本机复制文件夹时递归文件数超过此值则拦截推送、仅本地提示。
    /// 0 表示不限制。配置加载/保存时由 `set_max_folder_files` 同步。
    max_folder_files: Mutex<usize>,
    /// 已建立加密通道的对端（key 为 device_id）
    peers: Mutex<HashMap<String, Peer>>,
    /// 当前已连地址集合（key = remote `host:port`），用于 mDNS 失效时按手动/已知地址
    /// 兜底重连的去重：避免对已连地址反复发起连接尝试。
    connected_addrs: Mutex<HashSet<String>>,
    /// 正在发起连接的对端地址集合，避免重复连接（key = addr:port）
    connecting: Mutex<HashSet<String>>,
    /// 本端作为发送方的活动传输（transfer_id -> 本地文件清单 + 绝对路径）
    active_offers: Mutex<HashMap<String, OfferState>>,
    /// 本端作为接收方收到的待拉取清单（transfer_id -> 对端 offer）
    pending_offers: Mutex<HashMap<String, PendingOffer>>,
    /// 本端正在拉取的传输（transfer_id -> 写入通道 + 落盘目录 + 进度）
    active_pulls: Mutex<HashMap<String, PullState>>,
    /// 文件分片大小（字节）
    chunk_size: usize,
    /// 本端 WebSocket 监听端口，通过 Hello 告知对端，使应答方也能拼出对端可拨号地址。
    listen_port: std::sync::atomic::AtomicU16,
    /// 应用句柄（用于广播连接状态事件 / 写注册表 / 解析下载目录）
    app: Mutex<Option<AppHandle>>,
    /// 网格中继去重表（key = 剪贴板消息 id），防止连通图中消息无限回环。
    seen: Mutex<RelaySeen>,
}

impl ConnectionHub {
    pub fn new(identity: Arc<DeviceIdentity>, engine: Arc<SyncEngine>) -> Arc<Self> {
        Arc::new(Self {
            identity,
            engine,
            paired_codes: Mutex::new(HashMap::new()),
            pairing_code: Mutex::new(String::new()),
            max_folder_files: Mutex::new(100),
            peers: Mutex::new(HashMap::new()),
            connected_addrs: Mutex::new(HashSet::new()),
            connecting: Mutex::new(HashSet::new()),
            active_offers: Mutex::new(HashMap::new()),
            pending_offers: Mutex::new(HashMap::new()),
            active_pulls: Mutex::new(HashMap::new()),
            chunk_size: 256 * 1024, // 256 KiB/片
            listen_port: std::sync::atomic::AtomicU16::new(0),
            app: Mutex::new(None),
            seen: Mutex::new(RelaySeen::new()),
        })
    }

    /// 同步设置中的「配对码」到内存（配置加载/保存时调用）。
    pub fn set_pairing_code(&self, code: String) {
        *self.pairing_code.lock().unwrap() = code;
    }

    /// 同步设置中的「文件夹文件数上限」到内存（配置加载/保存时调用）。
    pub fn set_max_folder_files(&self, n: usize) {
        *self.max_folder_files.lock().unwrap() = n;
    }

    /// 读取当前静态配对口令（供首配对握手使用）。
    pub fn pairing_code(&self) -> String {
        self.pairing_code.lock().unwrap().clone()
    }

    /// 网格中继去重：标记/查询某剪贴板消息是否已处理过。true = 已见过（应丢弃）。
    fn note_seen(&self, id: &str) -> bool {
        self.seen.lock().unwrap().note(id)
    }

    pub fn is_paired(&self, device_id: &str) -> bool {
        self.paired_codes.lock().unwrap().contains_key(device_id)
    }

    /// 对端身份迁移：对端从同一 `ip:port` 出现但换了 device_id（重建身份）时，
    /// 把 link secret 从旧 id 挪到新 id。link secret 是双方共享口令，与 device_id
    /// 无关，迁移后用新 id 即可静默重连，无需重新配对。同时把密钥链里的持久化口令
    /// 改存到新 id，保证重启后仍能恢复。返回是否确实发生了迁移。
    pub fn migrate_pairing(&self, app: &AppHandle, old_id: &str, new_id: &str) -> bool {
        if old_id == new_id {
            return false;
        }
        let secret = {
            let mut g = self.paired_codes.lock().unwrap();
            match g.remove(old_id) {
                Some(s) => {
                    g.insert(new_id.to_string(), s.clone());
                    Some(s)
                }
                None => None,
            }
        };
        if let Some(secret) = secret {
            // 持久化层：新 id 下写入口令，再删旧 id，保证重启后恢复得到
            crate::device::store::store_secret(app, new_id, &secret);
            crate::device::store::delete_secret(app, old_id);
            tracing::info!("对端身份已迁移：{old_id} -> {new_id}");
            true
        } else {
            false
        }
    }

    /// 启动时把磁盘上恢复出来的 link secret 装回内存，使已配对设备无需再次交互即可重连。
    pub fn restore_paired(&self, secrets: HashMap<String, String>) {
        if secrets.is_empty() {
            return;
        }
        tracing::info!("已恢复 {} 台设备的配对状态", secrets.len());
        self.paired_codes.lock().unwrap().extend(secrets);
    }

    /// 取消与某设备的配对：清掉 link secret 与持久化记录，并断开当前连接。
    ///
    /// 移除 `peers` 表项会 drop 该连接的发送端，加密循环随即结束——因此这里
    /// 不需要额外的中断信号。
    pub fn unpair(&self, device_id: &str) {
        self.paired_codes.lock().unwrap().remove(device_id);
        self.peers.lock().unwrap().remove(device_id);
        if let Some(app) = self.app.lock().unwrap().clone() {
            crate::device::store::delete_secret(&app, device_id);
            let state = app.state::<AppState>();
            state.registry.lock().remove(&DeviceId(device_id.to_string()));
            let devices = state.registry.lock().list();
            crate::device::store::save_devices(&app, &devices);
            let _ = app.emit("peer-unpaired", device_id);
        }
        tracing::info!("已取消与 {device_id} 的配对");
    }

    /// 返回当前已建立加密通道的对端 device_id 集合（供前端挂载时主动查询一次）。
    pub fn connected_peer_ids(&self) -> Vec<String> {
        self.peers.lock().unwrap().keys().cloned().collect()
    }

    /// 当前接收到的「待拉取」文件清单快照（前端挂载时查询一次，兜底事件丢失）。
    pub fn pending_offers_snapshot(&self) -> Vec<serde_json::Value> {
        self.pending_offers
            .lock()
            .unwrap()
            .values()
            .map(|o| {
                serde_json::json!({
                    "transfer_id": o.transfer_id,
                    "device_id": o.device_id,
                    "device_name": o.device_name,
                    "top_names": o.top_names,
                    "has_folder": o.has_folder,
                    "files": o.files.iter().map(|f| serde_json::json!({
                        "file_name": f.file_name,
                        "file_size": f.file_size,
                        "is_dir": f.is_dir,
                        "relative_path": f.relative_path,
                    })).collect::<Vec<_>>(),
                    "total_size": o.files.iter().map(|f| f.file_size).sum::<u64>(),
                    "auto_pull": o.files.iter().map(|f| f.file_size).sum::<u64>()
                        < self.auto_pull_threshold_bytes(),
                })
            })
            .collect()
    }

    /// 解析文件同步落盘目录：配置优先，否则回退系统下载目录。
    fn resolve_sync_dir(&self, app: Option<&AppHandle>) -> PathBuf {
        let configured = app.and_then(|a| a.state::<AppState>().config.lock().sync_dir.clone());
        if let Some(dir) = configured {
            if !dir.trim().is_empty() {
                return PathBuf::from(dir.trim());
            }
        }
        if let Some(a) = app {
            if let Ok(dir) = a.path().download_dir() {
                return dir;
            }
        }
        PathBuf::from("Downloads")
    }

    /// 自动拉取阈值（字节）：各端自行配置 `auto_pull_threshold_mb`（默认 1MB）。
    /// 对端传来的传输若总大小小于此值，本端收到后直接自动拉取，无需手动点「拉取」。
    fn auto_pull_threshold_bytes(&self) -> u64 {
        self.app
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.state::<AppState>().config.lock().auto_pull_threshold_bytes())
            .unwrap_or(1024 * 1024)
    }

    /// 本地拷贝了文件/目录：生成传输 ID，展开为文件清单，向所有已连接对端广播「可拉取」。
    ///
    /// 绝对路径**只留在本端** `active_offers`，从不进网络；对端仅收到 `FileMeta` 元数据。
    pub fn offer_local_files(&self, paths: Vec<PathBuf>) {
        let existing: Vec<PathBuf> = paths.into_iter().filter(|p| p.exists()).collect();
        if existing.is_empty() {
            return;
        }
        let root = Self::common_root(&existing);
        let has_folder = existing.iter().any(|p| p.is_dir());
        let top_names: Vec<String> = existing
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect();
        // 文件夹文件数超过上限：递归计数，一旦超过上限立即返回（不展开清单、不广播、
        // 不写入 active_offers），仅在本地弹提示请用户压缩。无需算出精确总数。
        // 上限来自设置（max_folder_files），0 表示不限制。
        let max_folder_files = *self.max_folder_files.lock().unwrap();
        if max_folder_files > 0
            && has_folder
            && existing.iter().any(|p| Self::exceeds_file_limit(p, max_folder_files))
        {
            let folder_name = if top_names.is_empty() {
                "该".to_string()
            } else {
                top_names.join("、")
            };
            tracing::warn!(
                "文件夹 {folder_name} 文件数量超过 {max_folder_files}，已取消广播，请压缩后复制"
            );
            if let Some(app) = self.app.lock().unwrap().clone() {
                let _ = app.emit(
                    "file-count-exceeded",
                    serde_json::json!({ "folder_name": folder_name }),
                );
            }
            return;
        }
        let mut files: Vec<FileMeta> = Vec::new();
        let mut local_paths: Vec<PathBuf> = Vec::new();
        for p in &existing {
            if p.is_dir() {
                // 目录：递归展开为其中的所有文件（保留相对结构，含子文件夹）
                Self::collect_files_recursive(p, &root, &mut files, &mut local_paths);
            } else if p.is_file() {
                if let Some(meta) = Self::build_file_meta(p, &root) {
                    local_paths.push(p.clone());
                    files.push(meta);
                }
            }
        }
        if files.is_empty() {
            return;
        }
        let transfer_id = Uuid::new_v4().to_string();
        let my_id = self.identity.id.0.clone();
        let my_name = self.identity.name.clone();
        self.active_offers.lock().unwrap().insert(
            transfer_id.clone(),
            OfferState {
                device_name: my_name.clone(),
                files: files.clone(),
                local_paths,
            },
        );
        let peers = self.peers.lock().unwrap().clone();
        if peers.is_empty() {
            tracing::warn!("本地拷贝了文件，但当前没有已连接对端，未广播可拉取清单");
            return;
        }
        let frame = FileFrame::Offer {
            transfer_id: transfer_id.clone(),
            device_id: my_id,
            device_name: my_name,
            files,
            top_names: top_names.clone(),
            has_folder,
        };
        for (id, p) in &peers {
            if p.tx.send(Outgoing::File(frame.clone())).is_err() {
                tracing::warn!("对端 {id} 发送通道关闭，可拉取清单未送达");
            }
        }
        tracing::info!("已广播文件可拉取清单 {transfer_id} 给 {} 个对端", peers.len());
    }

    /// 处理对端发来的文件帧（按角色路由：发送方流式发片，接收方落盘）。
    pub(crate) async fn handle_file_frame(
        self: Arc<Self>,
        ff: FileFrame,
        tx: &mpsc::UnboundedSender<Outgoing>,
        _peer_id: &str,
    ) {
        match ff {
            FileFrame::Offer {
                transfer_id,
                device_id,
                device_name,
                files,
                top_names,
                has_folder,
            } => {
                if device_id == self.identity.id.0 {
                    return; // 忽略自己（拷贝端不会收到自己的 Offer，这里双保险）
                }
                // 防御回环：若本 Offer 的文件集合指纹与本机正在 offer 的某份相同，
                // 说明这是「本机刚复制出去、被对端（可能运行旧版、未抑制自身回声）又
                // 广播回来」的回声。直接丢弃——不进待拉取清单、也不 emit 给前端，
                // 避免「本机复制的文件又出现在本机待拉取列表」的表现。
                // 用「文件名 + 大小」指纹（忽略 relative_path）：对端把本机文件落盘后再
                // offer 时根目录不同，relative_path 必然不一致，而 file_name + file_size
                // 足以在绝大多数场景下稳定标识「同一份复制的文件」。
                let incoming_fp = Self::offer_fingerprint(&files);
                let is_local_echo = self
                    .active_offers
                    .lock()
                    .unwrap()
                    .values()
                    .any(|o| Self::offer_fingerprint(&o.files) == incoming_fp);
                if is_local_echo {
                    tracing::debug!("忽略本机文件回环 Offer {transfer_id}（指纹命中 active_offers）");
                    return;
                }
                let total: u64 = files.iter().map(|f| f.file_size).sum();
                // 自动拉取阈值：小于阈值则本端收到后直接拉取，免手动点击。
                // 拷贝端自身已被上面的 device_id 守卫排除，所以这里一定是「其它端」。
                let auto_pull = total < self.auto_pull_threshold_bytes();
                self.pending_offers.lock().unwrap().insert(
                    transfer_id.clone(),
                    PendingOffer {
                        transfer_id: transfer_id.clone(),
                        device_id: device_id.clone(),
                        device_name: device_name.clone(),
                        files: files.clone(),
                        top_names: top_names.clone(),
                        has_folder,
                    },
                );
                if let Some(app) = self.app.lock().unwrap().clone() {
                    let _ = app.emit(
                        "file-offer",
                        serde_json::json!({
                            "transfer_id": transfer_id,
                            "device_id": device_id,
                            "device_name": device_name,
                            "top_names": top_names,
                            "has_folder": has_folder,
                            "files": files.iter().map(|f| serde_json::json!({
                                "file_name": f.file_name,
                                "file_size": f.file_size,
                                "is_dir": f.is_dir,
                                "relative_path": f.relative_path,
                            })).collect::<Vec<_>>(),
                            "total_size": total,
                            "auto_pull": auto_pull,
                        }),
                    );
                }
                // 需手动拉取的传输：弹出托盘小窗口供一键拉取（不抢焦点，置顶显示）
                if !auto_pull {
                    if let Some(app) = self.app.lock().unwrap().clone() {
                        Self::show_pull_toast(&app);
                    }
                }
                // 小于阈值的传输自动拉取：直接走与手动拉取相同的链路（写盘 + 写本机剪贴板）。
                if auto_pull {
                    let hub = self.clone();
                    let tid = transfer_id.clone();
                    tauri::async_runtime::spawn(async move {
                        hub.pull_files(tid).await;
                    });
                }
            }
            FileFrame::PullRequest {
                transfer_id,
                file_indices,
            } => {
                // 本端是发送方：流式读取本地文件并发片
                let offer = self.active_offers.lock().unwrap().get(&transfer_id).cloned();
                let Some(offer) = offer else {
                    tracing::warn!("收到拉取请求但本端没有该传输 {transfer_id}");
                    return;
                };
                let tx = tx.clone();
                let chunk_size = self.chunk_size;
                tauri::async_runtime::spawn(async move {
                    for &idx in &file_indices {
                        if idx >= offer.local_paths.len() {
                            continue;
                        }
                        let path = &offer.local_paths[idx];
                        if let Ok(mut file) = tokio::fs::File::open(path).await {
                            use tokio::io::AsyncReadExt;
                            let mut offset: u64 = 0;
                            let mut buf = vec![0u8; chunk_size];
                            loop {
                                match file.read(&mut buf).await {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        let data = buf[..n].to_vec();
                                        if tx
                                            .send(Outgoing::File(FileFrame::Chunk(
                                                FileChunkResponsePayload {
                                                    transfer_id: transfer_id.clone(),
                                                    file_index: idx,
                                                    offset,
                                                    data,
                                                },
                                            )))
                                            .is_err()
                                        {
                                            return;
                                        }
                                        offset += n as u64;
                                    }
                                    Err(e) => {
                                        tracing::warn!("读文件失败 {}: {e}", path.display());
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    let _ = tx.send(Outgoing::File(FileFrame::Complete {
                        transfer_id: transfer_id.clone(),
                    }));
                    tracing::info!("传输 {transfer_id} 分片发送完毕");
                });
            }
            FileFrame::PullCancel { transfer_id } => {
                self.active_offers.lock().unwrap().remove(&transfer_id);
                self.active_pulls.lock().unwrap().remove(&transfer_id);
            }
            FileFrame::Complete { transfer_id } => {
                let tx_opt = self
                    .active_pulls
                    .lock()
                    .unwrap()
                    .get(&transfer_id)
                    .map(|s| s.chunk_tx.clone());
                if let Some(ctx) = tx_opt {
                    let _ = ctx.send(None).await;
                }
            }
            FileFrame::Chunk(payload) => {
                let tx_opt = self
                    .active_pulls
                    .lock()
                    .unwrap()
                    .get(&payload.transfer_id)
                    .map(|s| s.chunk_tx.clone());
                if let Some(ctx) = tx_opt {
                    if ctx.send(Some(payload)).await.is_err() {
                        tracing::warn!("写盘任务已退出，丢弃分片");
                    }
                } else {
                    tracing::warn!("收到未知传输的分片 {}", payload.transfer_id);
                }
            }
        }
    }

    /// 接收方点击「拉取」：建立落盘任务、发拉取请求，分片到位后写入 `sync_dir`，
    /// 完成后自动把下载路径写进本机剪贴板（用户直接 Ctrl+V 即可粘贴）。
    /// 收到需手动拉取的 Offer 时，把预声明的 pull-toast 小窗口定位到屏幕右下角并弹出。
    /// 不抢焦点（conf 中 focus=false），由 alwaysOnTop 保证置顶可见。
    fn show_pull_toast(app: &AppHandle) {
        const WIN_W: i32 = 340;
        const WIN_H: i32 = 200;
        const MARGIN: i32 = 16;
        const TASKBAR: i32 = 48;
        let Some(w) = app.get_webview_window("pull-toast") else {
            return;
        };
        if let Ok(Some(mon)) = app.primary_monitor() {
            let s = mon.size();
            let x = (s.width as i32) - WIN_W - MARGIN;
            let y = (s.height as i32) - WIN_H - MARGIN - TASKBAR;
            let _ = w.set_position(tauri::Position::Physical(tauri::PhysicalPosition { x, y }));
        }
        let _ = w.show();
    }

    pub async fn pull_files(self: Arc<Self>, transfer_id: String) {
        let offer = {
            let mut g = self.pending_offers.lock().unwrap();
            match g.remove(&transfer_id) {
                Some(o) => o,
                None => {
                    tracing::warn!("拉取失败：找不到传输 {transfer_id}");
                    return;
                }
            }
        };
        let peer_tx = {
            let g = self.peers.lock().unwrap();
            match g.get(&offer.device_id) {
                Some(p) => p.tx.clone(),
                None => {
                    tracing::warn!("拉取失败：对端 {} 未连接", offer.device_id);
                    self.pending_offers
                        .lock()
                        .unwrap()
                        .insert(transfer_id.clone(), offer);
                    return;
                }
            }
        };
        let app = self.app.lock().unwrap().clone();
        let sync_dir = self.resolve_sync_dir(app.as_ref());
        let root = sync_dir
            .join(Self::sanitize_name(&offer.device_name))
            .join(&transfer_id);
        let total: u64 = offer.files.iter().map(|f| f.file_size).sum();
        // 预建目录结构
        for f in &offer.files {
            let target = root.join(&f.relative_path);
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<Option<FileChunkResponsePayload>>(32);
        self.active_pulls.lock().unwrap().insert(
            transfer_id.clone(),
            PullState {
                chunk_tx: chunk_tx.clone(),
                target_dir: root.clone(),
                files: offer.files.clone(),
                device_id: offer.device_id.clone(),
                total_bytes: total,
            },
        );
        let _ = peer_tx.send(Outgoing::File(FileFrame::PullRequest {
            transfer_id: transfer_id.clone(),
            file_indices: (0..offer.files.len()).collect(),
        }));
        if let Some(a) = &app {
            let _ = a.emit(
                "file-pull-start",
                serde_json::json!({
                    "transfer_id": transfer_id,
                    "device_name": offer.device_name,
                    "total_size": total,
                    "target_dir": root.to_string_lossy(),
                }),
            );
        }
        // 写盘任务
        let engine = self.engine.clone();
        let tid = transfer_id.clone();
        let device_name = offer.device_name.clone();
        // 捕获文件清单供完成事件携带（offer 即将被 move）
        let file_details: Vec<serde_json::Value> = offer
            .files
            .iter()
            .map(|f| serde_json::json!({
                "name": f.file_name,
                "size": f.file_size,
                "is_dir": f.is_dir,
            }))
            .collect();
        let targets: Vec<PathBuf> = offer
            .files
            .iter()
            .map(|f| root.join(&f.relative_path))
            .collect();
        tauri::async_runtime::spawn(async move {
            use tokio::io::AsyncSeekExt;
            use tokio::io::AsyncWriteExt;
            let mut written: u64 = 0;
            let mut received: Vec<PathBuf> = Vec::new();
            let mut last_progress_at = std::time::Instant::now();
            let mut last_progress_pct: u32 = 0;
            while let Some(chunk) = chunk_rx.recv().await {
                let Some(payload) = chunk else { break; };
                if payload.file_index < targets.len() {
                    let path = &targets[payload.file_index];
                        if let Ok(mut file) = tokio::fs::OpenOptions::new()
                            .create(true)
                            .write(true)
                            .truncate(false)
                            .open(path)
                            .await
                    {
                        let _ = file.seek(std::io::SeekFrom::Start(payload.offset)).await;
                        if file.write_all(&payload.data).await.is_ok() {
                            written += payload.data.len() as u64;
                            if !received.contains(path) {
                                received.push(path.clone());
                            }
                        }
                    }
                }
                // 节流上报进度：每 ≥5% 或间隔 ≥200ms 至多 emit 一次，避免事件洪峰
                let pct = (written * 100 / total.max(1)) as u32;
                let now = std::time::Instant::now();
                if pct.saturating_sub(last_progress_pct) >= 5
                    || now.duration_since(last_progress_at) >= std::time::Duration::from_millis(200)
                {
                    last_progress_pct = pct;
                    last_progress_at = now;
                    if let Some(a) = &app {
                        let _ = a.emit(
                            "file-pull-progress",
                            serde_json::json!({
                                "transfer_id": tid,
                                "received": written,
                                "total": total,
                                "percent": pct,
                            }),
                        );
                    }
                }
            }
            // 落盘完成：自动写本机剪贴板（带内容哈希回声抑制，避免触发新一轮 Offer 广播）
            if !received.is_empty() {
                // 抑制「拉取完成后自动写本机剪贴板」被本地监听误判为新的文件拷贝而回环广播：
                // 记录本次写出路径的哈希，处理任务在检测到变化、且读到的路径哈希与之一致时，
                // 即视为本机刚写入的回声而丢弃，不广播 Offer。与文本/图片经 `last_emitted`
                // 内容哈希去重的思路完全一致——基于内容哈希、不依赖监听线程的触发时机，
                // 彻底消除「Mac 复制 → Windows 拉取 → Mac 又看到同一文件」的回环。
                engine.suppress_next_file_offer(&received);
                let _ = engine.clipboard().write_file_paths(&received).await;
            }
            if let Some(a) = &app {
                let _ = a.emit(
                    "file-pull-complete",
                    serde_json::json!({
                        "transfer_id": tid,
                        "device_name": device_name,
                        "target_dir": root.to_string_lossy(),
                        "file_count": received.len(),
                        "files": file_details,
                        "pulled_at": now_secs(),
                    }),
                );
            }
            tracing::info!("拉取 {tid} 完成，共写入 {written} 字节");
        });
    }

    /// 计算多条路径的最长公共父目录（用于还原相对结构）
    fn common_root(paths: &[PathBuf]) -> PathBuf {
    let mut root: Option<PathBuf> = None;
    for p in paths {
        let parent = p.parent().unwrap_or(p).to_path_buf();
        root = Some(match root {
            None => parent,
            Some(r) => Self::common_prefix(&r, &parent),
        });
    }
    root.unwrap_or_else(|| PathBuf::from("/"))
}

    fn common_prefix(a: &std::path::Path, b: &std::path::Path) -> PathBuf {
    let mut res = PathBuf::new();
    for (x, y) in a.components().zip(b.components()) {
        if x == y {
            res.push(x.as_os_str());
        } else {
            break;
        }
    }
    res
}

    /// 由本地绝对路径构造 `FileMeta`（相对路径相对于公共根）
    fn build_file_meta(path: &std::path::Path, root: &std::path::Path) -> Option<FileMeta> {
    let meta = std::fs::metadata(path).ok()?;
    let relative_path = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let file_name = path.file_name()?.to_string_lossy().to_string();
    let modified_at = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some(FileMeta {
        file_name,
        file_size: meta.len(),
        is_dir: meta.is_dir(),
        relative_path,
        modified_at,
        mime_type: String::new(),
        hash: None,
    })
}

    /// 递归判断某路径下的文件数是否超过 `limit`：一旦超过立即返回 true，不继续遍历，
    /// 不计算精确总数。用于「文件夹文件数超限」拦截（超大目录也不做完整统计，避免卡顿）。
    fn exceeds_file_limit(path: &std::path::Path, limit: usize) -> bool {
        if path.is_file() {
            return 1 > limit;
        }
        let mut count: usize = 0;
        let mut stack: Vec<std::path::PathBuf> = Vec::new();
        if path.is_dir() {
            stack.push(path.to_path_buf());
        }
        while let Some(dir) = stack.pop() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let child = e.path();
                    if child.is_dir() {
                        stack.push(child);
                    } else if child.is_file() {
                        count += 1;
                        if count > limit {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// 递归展开目录下的所有文件（保留相对结构，含子文件夹），追加到传输清单。
    fn collect_files_recursive(
        dir: &std::path::Path,
        root: &std::path::Path,
        files: &mut Vec<FileMeta>,
        local_paths: &mut Vec<PathBuf>,
    ) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let child = e.path();
                if child.is_dir() {
                    Self::collect_files_recursive(&child, root, files, local_paths);
                } else if child.is_file() {
                    if let Some(meta) = Self::build_file_meta(&child, root) {
                        local_paths.push(child);
                        files.push(meta);
                    }
                }
            }
        }
    }


/// 计算一份文件清单的「回环指纹」：以「文件名 + 大小」集合（排序后哈希）标识，
/// 忽略 `relative_path`——因为对端把本机文件落盘后再 offer 时根目录不同，
/// `relative_path` 必然不一致，而 `file_name + file_size` 足以在绝大多数场景下
/// 稳定标识「同一份复制的文件」。用于接收方识别「本机刚复制出去、被对端回环广播回来」的回声。
fn offer_fingerprint(files: &[FileMeta]) -> String {
    use std::hash::{Hash, Hasher};
    let mut v: Vec<(String, u64)> = files
        .iter()
        .map(|f| (f.file_name.clone(), f.file_size))
        .collect();
    v.sort();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut h);
    format!("{:x}", h.finish())
}

    /// 把设备名等非安全字符净化为目录名
    fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

    /// 启动监听器、本地剪贴板广播器，并订阅 mDNS 发现事件（仅对已配对对端自动重连）。
    pub async fn start(self: Arc<Self>, app: AppHandle, listen_port: u16) {
        *self.app.lock().unwrap() = Some(app.clone());
        self.listen_port
            .store(listen_port, std::sync::atomic::Ordering::Relaxed);

        // 1) 本地剪贴板变化广播器：订阅引擎事件，加密转发给所有对端
        {
            let engine = self.engine.clone();
            let hub = self.clone();
            tauri::async_runtime::spawn(async move {
                let mut rx = engine.subscribe();
                loop {
                    // 不能用 `while let Ok(ev)`：broadcast 落后时返回 `Lagged`，那样会让
                    // 转发任务**永久退出**，本机从此只收不发（接收走 run_connection 里
                    // 另一条路径，仍然正常）——正是「单向能同步」的典型成因。
                    let ev = match rx.recv().await {
                        Ok(ev) => ev,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("转发任务积压，跳过 {n} 条本地变化");
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };
                    match ev {
                        SyncEvent::LocalClipboardChanged { mark, content } => {
                            let msg_id = Uuid::new_v4().to_string();
                            let env = SyncEnvelope {
                                mark,
                                content,
                                msg_id: msg_id.clone(),
                                ttl: RELAY_MAX_TTL,
                            };
                            // 记录本机生成的消息 id，避免经其他路径回传时被重复应用/转发。
                            hub.note_seen(&msg_id);
                            let peers = hub.peers.lock().unwrap().clone();
                            if peers.is_empty() {
                                tracing::warn!("本地剪贴板已变化，但当前没有已连接对端，内容未发出");
                                continue;
                            }
                            tracing::debug!(
                                "向 {} 个对端转发本地剪贴板变化 (relay ttl={RELAY_MAX_TTL})",
                                peers.len()
                            );
                            for (id, p) in peers {
                                if p.tx.send(Outgoing::Sync(env.clone())).is_err() {
                                    tracing::warn!("对端 {id} 的发送通道已关闭，本次内容未送达");
                                }
                            }
                        }
                        SyncEvent::LocalFilesCopied { paths } => {
                            // 本地拷贝了文件/目录：广播「可拉取」清单给所有对端。
                            // 注意不把绝对路径外传——对端只拿到元数据，文件内容走拉取。
                            hub.offer_local_files(paths);
                        }
                        SyncEvent::RemoteClipboardApplied { .. } => {
                            // 来自对端的剪贴板更新已在本机落盘/写剪贴板，无需再转发回去
                            // （回环由引擎的 last_emitted 哈希去重负责，且 manager 不参与
                            // 本地→对端的转发，仅 LocalClipboardChanged 走此路径）。
                        }
                    }
                }
            });
        }

        // 2) 监听入站连接（作为 SPAKE2 应答方）。
        //    同一端口同时承载跨 LAN 文件 HTTP 拉取：accept 后先嗅探请求行，
        //    `GET /file/` 走文件服务，其余按 WebSocket 握手处理。
        {
            let hub = self.clone();
            let file_share = app.state::<crate::AppState>().file_share.clone();
            let network_key = app.state::<crate::AppState>().network_key.clone();
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
                                    // 嗅探请求行：文件拉取走 HTTP 文件服务，其余升级为 WS
                                    let mut peek_buf = [0u8; 512];
                                    let n = sock.peek(&mut peek_buf).await.unwrap_or(0);
                                    let is_file = {
                                        let slice = &peek_buf[..n];
                                        if let Some(lf) =
                                            slice.windows(2).position(|w| w == b"\r\n")
                                        {
                                            String::from_utf8_lossy(&slice[..lf])
                                                .starts_with("GET /file/")
                                        } else {
                                            false
                                        }
                                    };
                                    if is_file {
                                        let fs = file_share.clone();
                                        let nk = network_key.clone();
                                        tauri::async_runtime::spawn(async move {
                                            crate::file_server::handle_file_stream(sock, fs, nk)
                                                .await;
                                        });
                                        continue;
                                    }
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

        // 3) 订阅 mDNS 发现事件：仅当对端已配对时才自动连接（强制交互式——
        //    未配对对端必须用户在前端手动发起配对，绝不静默自动连接）。
        //    这条路径只为「刚发现就立刻连上」的响应速度服务，可靠性由下面的监控任务兜底。
        {
            let hub = self.clone();
            app.listen("peer-discovered", move |event| {
                let payload = event.payload().to_string();
                if let Ok(peer) = serde_json::from_str::<DiscoveredPeer>(&payload) {
                    hub.clone().spawn_reconnect_if_paired(peer);
                }
            });
        }

        // 4) 连接监控：周期巡检「已配对 + 已发现/已知地址 + 未连接」的对端并重连。
        //    不能只靠上面的发现事件——进程重启后 mDNS 可能早已把对端解析完毕、
        //    不再重复推送，那样已配对设备会一直停在离线状态。
        {
            let hub = self.clone();
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(RECONNECT_INTERVAL).await;
                    let state = app.state::<AppState>();

                    // 4a) mDNS 发现到的对端（仅已配对的才连）
                    {
                        let peers: Vec<DiscoveredPeer> = {
                            let g = state.discovered.lock();
                            g.values().cloned().collect()
                        };
                        for peer in peers {
                            hub.clone().spawn_reconnect_if_paired(peer);
                        }
                    }

                    // 4b) 手动地址簿：mDNS 被防火墙拦截时的兜底直连。
                    //     只有对端确实已配对（握手阶段命中 link secret）才会真正连上，
                    //     指向陌生人的手动地址会在握夹持早失败，不会误连。
                    {
                        let manual: Vec<ManualAddress> = state.manual.lock().list();
                        for m in manual {
                            hub.clone().spawn_connect_addr(m.addr.clone(), m.port);
                        }
                    }

                    // 4c) 已配对设备的最后已知地址：配对成功后由发起方记录，
                    //     mDNS 失效也能自愈重连（重启后从磁盘恢复）。
                    {
                        let paired = state.registry.lock().list();
                        for d in paired {
                            if let Some(addr) = d.last_addr {
                                if hub.peers.lock().unwrap().contains_key(&d.device_id.0) {
                                    continue; // 当前已连，跳过
                                }
                                if let Some((h, p)) = parse_host_port(&addr) {
                                    hub.clone().spawn_connect_addr(h, p);
                                }
                            }
                        }
                    }
                }
            });
        }
    }

    /// 若对端已配对且当前既没连上、也没有正在进行的连接尝试，则后台发起一次重连。
    ///
    /// 幂等：重复调用（发现事件 + 定时巡检同时命中）不会产生并发连接，
    /// 由 `connecting` 守卫按 `addr:port` 去重。
    fn spawn_reconnect_if_paired(self: Arc<Self>, peer: DiscoveredPeer) {
        if !self.is_paired(&peer.device_id) {
            return;
        }
        if self.peers.lock().unwrap().contains_key(&peer.device_id) {
            return;
        }
        let key = format!("{}:{}", peer.addr, peer.port);
        if self.connected_addrs.lock().unwrap().contains(&key) {
            return;
        }
        if self.connecting.lock().unwrap().contains(&key) {
            return;
        }
        tauri::async_runtime::spawn(async move {
            self.reconnect_once(peer).await;
        });
    }

    /// 按地址直接尝试连接（不预检配对状态）。
    ///
    /// 只有对端在握手阶段被识别为「已配对」（Hello 拿到的真实 device_id 命中 link
    /// secret 缓存）才会真正建立通道；未配对对端会因「未提供配对码」在握夹持早失败，
    /// 不会误连陌生人。用于 mDNS 失效时的兜底：手填的手动地址 + 已配对设备的最后已知地址。
    ///
    /// 由 `connected_addrs` / `connecting` 按 `addr:port` 双重去重，避免对已连或正在
    /// 连接中的地址反复发起尝试。
    fn spawn_connect_addr(self: Arc<Self>, addr: String, port: u16) {
        let key = format!("{addr}:{port}");
        if self.connected_addrs.lock().unwrap().contains(&key) {
            return;
        }
        if self.connecting.lock().unwrap().contains(&key) {
            return;
        }
        let peer = DiscoveredPeer {
            device_id: String::new(),
            device_name: "手动/已知地址".to_string(),
            addr,
            port,
        };
        tauri::async_runtime::spawn(async move {
            self.reconnect_once(peer).await;
        });
    }

    /// 用户在前端手动发起配对：作为发起方，使用用户输入的对方配对码连接对端。
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
                Err(e) => {
                    // 口令不一致是确定性失败：同一个码再试多少次都不会成功，
                    // 继续重试只会每 5 秒重复弹一次错误。立即终止并给出明确提示。
                    if e.downcast_ref::<HandshakeError>().is_some() {
                        tracing::warn!("pairing with {} rejected: {e}", peer.device_name);
                        self.connecting.lock().unwrap().remove(&key);
                        if let Some(app) = self.app.lock().unwrap().clone() {
                            let reason = if self.is_paired(&peer.device_id) {
                                "重连失败：与对方配对状态不一致（本机已保存其配对信息但密钥不匹配），请双方先「取消配对」再重新配对".to_string()
                            } else {
                                "配对码不一致，请核对对方界面显示的配对码".to_string()
                            };
                            let _ = app.emit(
                                "pairing-failed",
                                serde_json::json!({
                                    "device_id": peer.device_id,
                                    "reason": reason,
                                }),
                            );
                        }
                        return;
                    }
                    // 对端不可达（地址/端口错误、防火墙、WebSocket 握手未完成）：
                    // 首配对时立即反馈，避免傻等 MAX_PAIRING_ATTEMPTS×5s 才报「配对超时」，
                    // 否则用户点确认后长时间无结果，感知为「确认没反应」。
                    let emsg = e.to_string();
                    if emsg.contains("connect failed")
                        || emsg.contains("Handshake")
                        || emsg.contains("handshake")
                    {
                        tracing::warn!("pairing with {} unreachable: {e}", peer.device_name);
                        self.connecting.lock().unwrap().remove(&key);
                        if let Some(app) = self.app.lock().unwrap().clone() {
                            let _ = app.emit(
                                "pairing-failed",
                                serde_json::json!({
                                    "device_id": peer.device_id,
                                    "reason": "对端不可达：请确认对方在线、IP/端口正确（双方监听端口需一致，当前 20071），且未被防火墙拦截",
                                }),
                            );
                        }
                        return;
                    }
                    tracing::warn!(
                        "pair attempt {} to {} failed: {e}",
                        attempt + 1,
                        peer.device_name
                    );
                }
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
                        "reason": "配对超时，请确认对方已上线、端口可达且你输入的是对方界面显示的配对码",
                    }),
                );
            }
        }
    }

    /// 通过手动地址（跨网络 / mDNS 被拦截）发起首配对：构造一个对端描述，
    /// 用调用方传入的对方配对码作为 SPAKE2 口令连接对端。device_id 留空，
    /// 连接后由 Hello 阶段的真实身份覆盖。两端配对码各自独立、无需预先相同。
    pub async fn pair_with_manual_address(self: Arc<Self>, addr: String, port: u16, code: String) {
        let peer = DiscoveredPeer {
            device_id: String::new(),
            device_name: addr.clone(),
            addr,
            port,
        };
        self.pair_with(peer, code).await;
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

    /// 对已配对对端发起**一次**连接尝试（作为 SPAKE2 发起方），用缓存的 link secret
    /// 静默握手；连接建立后会一直跑到断开才返回。
    ///
    /// 这里刻意不做无限重连循环——重试统一由监控任务驱动。否则对端换了 IP 之后，
    /// 旧循环会一直占着按 `addr:port` 建立的守卫空转，而它永远也连不上了。
    async fn reconnect_once(self: Arc<Self>, peer: DiscoveredPeer) {
        let key = format!("{}:{}", peer.addr, peer.port);
        if !self.connecting.lock().unwrap().insert(key.clone()) {
            return;
        }
        if let Err(e) = self.clone().connect_once(peer.clone(), None).await {
            tracing::debug!("与 {} 的连接结束：{e}", peer.device_name);
        }
        self.connecting.lock().unwrap().remove(&key);
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
        let my_id = self.identity.id.0.clone();
        let my_hello = HelloPayload {
            device_id: my_id.clone(),
            device_name: self.identity.name.clone(),
            public_key: base64::engine::general_purpose::STANDARD
                .encode(self.identity.public_key_bytes()),
            listen_port: self
                .listen_port
                .load(std::sync::atomic::Ordering::Relaxed),
        };
        send_frame(&mut ws, MessageType::Hello, &serde_json::to_vec(&my_hello)?).await?;
        let (_ht, hpayload) = recv_frame(&mut ws).await?;
        let peer_hello: HelloPayload = serde_json::from_slice(&hpayload)
            .map_err(|e| anyhow::anyhow!("bad hello payload: {e}"))?;
        let peer_id = peer_hello.device_id.clone();
        let peer_name = peer_hello.device_name.clone();

        // 防御：拒绝与自身建立连接（自连时双方 id 相同，会让口令确认失去意义）
        if peer_id == my_id {
            anyhow::bail!("拒绝与自身建立连接");
        }

        // 2) 选择口令：已配对 → 用持久化的 link secret 静默重连（含重启后）；
        //    未配对（首配对）→ 应答方用本机「配对码」（应答方常驻、无需点击生成），
        //    发起方必须用调用方显式传入的对方配对码（outgoing_code）；两端配对码
        //    各自独立、无需预先相同，仅本次配对握手持平即可。刷新本机配对码不影响
        //    已配对设备（它们走 link secret 重连）。
        let cached = self.paired_codes.lock().unwrap().get(&peer_id).cloned();
        let (pw, is_fresh_pairing) = match cached {
            Some(secret) => (secret, false),
            None => {
                let code = match role {
                    Role::Initiator => outgoing_code.clone().ok_or_else(|| {
                        anyhow::anyhow!("未提供配对码（请在配对时输入对方显示的配对码）")
                    })?,
                    Role::Responder => self.pairing_code(),
                };
                (code, true)
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

        // 4) HMAC 校验：确认两端使用同一口令（错误口令会派生不同密钥 → 校验失败）。
        //
        // 关键：标签必须**各自绑定发送者身份**，而不是绑定接收者。每端发送
        // `HMAC(key, ctx || 自己的 id)`，并用 `HMAC(key, ctx || 对端 id)` 去核对收到的值。
        // 若两端都用「对端 id」做输入再直接比较，即使密钥完全一致，因 id 不同，
        // 两个标签也永远不等 —— 那样配对会 100% 失败。
        let my_tag = verify_tag(&key, &my_id);
        let expected_peer_tag = verify_tag(&key, &peer_id);
        send_frame(&mut ws, MessageType::Verify, &my_tag).await?;
        let (_vt, peer_tag) = recv_frame(&mut ws).await?;
        if !ct_eq(&peer_tag, &expected_peer_tag) {
            // HMAC 校验失败：两端使用的口令不一致。需区分两种情形给出不同提示，
            // 否则会自动重连（link secret）与首次配对（输入的配对码）的失败被混为一谈，
            // 在「一端已配对、另一端配对状态已丢失」的非对称情况下误导用户。
            if matches!(role, Role::Responder) {
                if is_fresh_pairing {
                    // 首次配对：用户确实输入了对方配对码，但两边不一致 → 如实提示。
                    if let Some(app) = self.app.lock().unwrap().clone() {
                        let _ = app.emit(
                            "pairing-failed",
                            serde_json::json!({
                                "device_id": peer_id,
                                "reason": format!("与「{peer_name}」的配对码不一致"),
                            }),
                        );
                    }
                } else {
                    // 自动重连（link secret）失败：多半是两端配对状态不一致
                    // （一端配置被重置/清除）。这属于后台静默重连，不该弹「配对码不一致」
                    // 误导用户；只记日志，由用户侧「取消配对后重新配对」来修复。
                    tracing::warn!(
                        "与 {peer_name} 的重连握手失败：本机与其配对状态不一致（link secret 不匹配），建议双方先取消配对再重新配对"
                    );
                }
            }
            return Err(HandshakeError::CodeMismatch.into());
        }

        if is_fresh_pairing {
            tracing::info!("paired with {} ({})", peer_name, peer_addr);
        } else {
            tracing::info!("reconnected to {} ({})", peer_name, peer_addr);
        }

        // 5) 登记为已配对：持久化 link secret + 写注册表并落盘 + 通知前端
        if let Some(app) = self.app.lock().unwrap().clone() {
            let pk = base64::engine::general_purpose::STANDARD
                .decode(&peer_hello.public_key)
                .map_err(|e| anyhow::anyhow!("bad peer public key: {e}"))?;
            let fingerprint = sha256_hex(&pk).get(..32).unwrap_or("").to_string();

            // 只有**首次配对**才派生并保存 link secret。重连时会话密钥是用 link secret
            // 本身派生的，若此时再派生一次并覆盖，两端一旦有一方保存失败就会永久失配。
            if is_fresh_pairing {
                let link = derive_link_secret(&key);
                self.paired_codes
                    .lock()
                    .unwrap()
                    .insert(peer_id.clone(), link.clone());
                crate::device::store::store_secret(&app, &peer_id, &link);
            }

            let state = app.state::<AppState>();
            // 双方每次连上（无论发起还是应答）都用「对端声明 listen_port」刷新记录：
            // A↔B 任意一方主动连上都会走到这里，互相更新对方信息。这样即使旧记录里残留
            // 坏地址（如 127.0.0.1:旧端口），下次重连后也能自愈——端口被刷成最新 listen_port。
            // - 端口：一律以 Hello 里的 listen_port 为准（旧版未带 =0 时无法刷新，保留原值）。
            // - host：优先用本次 TCP 连接的对端真实源 IP；但若该 IP 是回环地址、而旧记录里
            //   已有一个非回环地址，则保留旧 host、只更新端口（避免把外网可达地址改回 127.0.0.1）。
            let new_host = peer_addr
                .rsplit_once(':')
                .map(|(h, _)| h)
                .unwrap_or("")
                .to_string();
            let new_port = peer_hello.listen_port;
            let existing_addr = state
                .registry
                .lock()
                .get(&DeviceId(peer_id.clone()))
                .and_then(|d| d.last_addr.clone());
            let last_addr = if new_port == 0 {
                existing_addr.clone() // 旧版未带 listen_port，无法刷新端口，保留已有
            } else {
                let (ex_host, _) = existing_addr
                    .as_ref()
                    .and_then(|a| a.rsplit_once(':'))
                    .map(|(h, p)| (h.to_string(), p.to_string()))
                    .unwrap_or_default();
                let is_new_loopback = new_host == "127.0.0.1" || new_host == "::1";
                let is_ex_loopback = ex_host == "127.0.0.1" || ex_host == "::1";
                let host = if (is_new_loopback && !is_ex_loopback && !ex_host.is_empty())
                    || new_host.is_empty()
                {
                    ex_host // 新源是回环且旧记录有真实地址，或解析不出 host：保留旧 host，只更新端口
                } else {
                    new_host.clone() // 用本次真实源 IP（自愈：即便旧的是坏地址也会被覆盖）
                };
                Some(format!("{host}:{new_port}"))
            };
            state.registry.lock().add(PairedDevice {
                device_id: DeviceId(peer_id.clone()),
                device_name: peer_name.clone(),
                fingerprint: fingerprint.clone(),
                trust: TrustLevel::Verified,
                last_seen: now_secs(),
                last_addr: last_addr.clone(),
            });
            let devices = state.registry.lock().list();
            crate::device::store::save_devices(&app, &devices);

            // 记录当前已连地址，供兜底重连去重（同样只在发起方有效，
            // 此时 peer_addr 才是对端真实监听地址 host:port）。
            if matches!(role, Role::Initiator) {
                self.connected_addrs.lock().unwrap().insert(peer_addr.clone());
            }

            let _ = app.emit(
                "peer-connected",
                serde_json::json!({
                    "device_id": peer_id,
                    "device_name": peer_name,
                    "addr": peer_addr,
                }),
            );
            // 首次配对才通知前端「已配对」——重连时设备已在已配对列表中，
            // 不应重复弹"已与 X 配对"提示；重连只需 peer-connected 即可。
            if is_fresh_pairing {
                let _ = app.emit(
                    "peer-paired",
                    serde_json::json!({
                        "id": peer_id,
                        "name": peer_name,
                        "fingerprint": fingerprint,
                        "trusted": true,
                        "last_seen": now_secs(),
                        "last_addr": last_addr,
                    }),
                );
            } else {
                // 重连时设备信息可能已更新（改名 / 换地址），用 peer-info-updated
                // 通知前端刷新展示，但不触发"已配对"提示
                let _ = app.emit(
                    "peer-info-updated",
                    serde_json::json!({
                        "id": peer_id,
                        "name": peer_name,
                        "fingerprint": fingerprint,
                        "trusted": true,
                        "last_seen": now_secs(),
                        "last_addr": last_addr,
                    }),
                );
            }
            // 首配对成功后 link secret 已持久化；静态配对口令长期有效，无需解除武装。
        }

        // 6) 注册对端，进入加密同步循环
        let conn_id = NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed);
        // 发送端**只**存放在 peers 表里（不在本地留 clone）：这样取消配对时
        // 把表项一移除，tx 即被 drop，下面的 rx 立刻收到 None 并结束本连接。
        let (tx, mut rx) = mpsc::unbounded_channel::<Outgoing>();
        // handle_file_frame 需要在解密循环里用发送端回传分片，而 tx 已被移入 Peer
        // 表项（用于转发任务）。此处克隆一份专供文件帧回传，避免「move 后借用」。
        let tx_for_file = tx.clone();
        if let Some(old) = self
            .peers
            .lock()
            .unwrap()
            .insert(peer_id.clone(), Peer { conn_id, tx })
        {
            tracing::debug!(
                "与 {peer_name} 的连接 #{} 已被新连接 #{conn_id} 取代",
                old.conn_id
            );
        }

        let (mut write, mut read) = ws.split();

        // 加密消息循环
        loop {
            tokio::select! {
                outgoing = rx.recv() => {
                    match outgoing {
                        Some(out) => {
                            // Sync / File 两类帧统一在此用会话密钥加密后发出
                            let (msg_type, pt) = match out {
                                Outgoing::Sync(env) => (
                                    MessageType::Sync,
                                    bincode::serialize(&env)
                                        .map_err(|e| anyhow::anyhow!("serialize envelope: {e}"))?,
                                ),
                                Outgoing::File(ff) => (
                                    MessageType::File,
                                    bincode::serialize(&ff)
                                        .map_err(|e| anyhow::anyhow!("serialize file frame: {e}"))?,
                                ),
                            };
                            let nonce: [u8; 12] = rand::random();
                            let ct = encrypt(&key, &nonce, &pt)
                                .map_err(|e| anyhow::anyhow!("encrypt: {e}"))?;
                            let mut payload = Vec::with_capacity(12 + ct.len());
                            payload.extend_from_slice(&nonce);
                            payload.extend_from_slice(&ct);
                            let frame = MessageFrame::new(msg_type, payload).encode();
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
                                match f.msg_type {
                                    MessageType::Sync => {
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
                                                    // 网格中继：去重 → 本地应用 → 转发给其他直连对端
                                                    if self.note_seen(&env.msg_id) {
                                                        tracing::debug!(
                                                            "剪贴板消息 {} 已处理过，跳过（防网格回环）",
                                                            env.msg_id
                                                        );
                                                        continue;
                                                    }
                                                    let SyncEnvelope { mark, content, msg_id, ttl } = env;
                                                    self.engine.apply_remote(mark.clone(), content.clone()).await;
                                                    if ttl > 1 {
                                                        let relay = SyncEnvelope {
                                                            mark,
                                                            content,
                                                            msg_id: msg_id.clone(),
                                                            ttl: ttl - 1,
                                                        };
                                                        let peers =
                                                            self.peers.lock().unwrap().clone();
                                                        let other_count = peers.len().saturating_sub(1);
                                                        for (id, p) in peers {
                                                            if id == peer_id {
                                                                continue;
                                                            }
                                                            if p.tx.send(Outgoing::Sync(relay.clone())).is_err() {
                                                                tracing::warn!(
                                                                    "中继转发对端 {id} 通道已关闭"
                                                                );
                                                            }
                                                        }
                                                        tracing::debug!(
                                                            "中继剪贴板消息 {msg_id} 至其他 {other_count} 个对端 (ttl={})",
                                                            ttl - 1
                                                        );
                                                    }
                                                }
                                            }
                                            Err(e) => tracing::warn!("decrypt failed: {e}"),
                                        }
                                    }
                                    MessageType::File => {
                                        if f.payload.len() < 12 {
                                            continue;
                                        }
                                        let (nonce, ct) = f.payload.split_at(12);
                                        let mut n = [0u8; 12];
                                        n.copy_from_slice(nonce);
                                        match decrypt(&key, &n, ct) {
                                            Ok(pt) => {
                                                if let Ok(ff) =
                                                    bincode::deserialize::<FileFrame>(&pt)
                                                {
                                                    self.clone().handle_file_frame(ff, &tx_for_file, &peer_id).await;
                                                }
                                            }
                                            Err(e) => tracing::warn!("file decrypt failed: {e}"),
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        _ => {}
                    }
                }
            }
        }

        // 清理：仅当表中登记的仍是**本条**连接时才摘除。若期间已被更新的连接取代，
        // 无条件 remove 会把活着的新连接一并删掉，本机将只收不发。
        let was_current = {
            let mut g = self.peers.lock().unwrap();
            match g.get(&peer_id) {
                Some(p) if p.conn_id == conn_id => {
                    g.remove(&peer_id);
                    true
                }
                _ => false,
            }
        };
        if was_current {
            if let Some(app) = self.app.lock().unwrap().clone() {
                let _ = app.emit("peer-disconnected", &peer_id);
            }
            tracing::info!("connection to {} closed", peer_name);
        } else {
            tracing::debug!("{peer_name} 的旧连接 #{conn_id} 退出，当前连接不受影响");
        }
        // 移出已连地址集合，允许监控任务在断线后重新兜底重连
        self.connected_addrs.lock().unwrap().remove(&peer_addr);
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

/// 生成口令确认标签：`HMAC(session_key, VERIFY_CONTEXT || device_id)`。
///
/// 每端用**自己的 id** 生成待发送标签，用**对端 id** 生成期望值来核对，
/// 保证两端计算出的两个标签一一对应；口令不同则会话密钥不同，标签必然对不上。
fn verify_tag(key: &[u8; 32], device_id: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts 32-byte key");
    mac.update(VERIFY_CONTEXT);
    mac.update(device_id.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// 派生长期重连口令：`hex(HMAC(session_key, LINK_CONTEXT))`。
///
/// 两端会话密钥相同 → 派生结果必然相同，因此双方无需再交换任何东西即可各自
/// 保存同一个口令。用它替代用户输入的 6 位码有两点好处：熵高得多（256 bit
/// 而非 20 bit），且用户可见的配对码不会变成长期凭据。
fn derive_link_secret(key: &[u8; 32]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts 32-byte key");
    mac.update(LINK_CONTEXT);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// 定长常量时间比较，避免通过比较耗时泄漏标签前缀信息。
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
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

/// 从 `host:port` 解析出 (host, port)；空串或端口非法时返回 None。
/// 取最后一个 `:` 作为分隔，兼容大多数 IPv4/域名地址（IPv6 不在 MVP 局域网范围内）。
fn parse_host_port(s: &str) -> Option<(String, u16)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (h, p) = s.rsplit_once(':')?;
    let port: u16 = p.parse().ok()?;
    if h.is_empty() {
        return None;
    }
    Some((h.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归测试：同一配对码下，两端**交叉核对**的口令确认标签必须相等。
    ///
    /// 早期实现两端都用「对端 id」生成标签后直接相互比较，即使会话密钥一致，
    /// 由于 id 不同，标签也永远不等，导致配对 100% 报「配对码不一致」。
    #[test]
    fn verify_tags_cross_match_with_same_code() {
        let pw = "123456";
        let init = start_initiator(pw);
        let resp = start_responder(pw);
        let init_msg = init.message.clone();
        let k_a = derive_session_key(&init.finish(&resp.message).unwrap()).unwrap();
        let k_b = derive_session_key(&resp.finish(&init_msg).unwrap()).unwrap();
        assert_eq!(k_a, k_b, "相同配对码应派生出相同会话密钥");

        let (id_a, id_b) = ("device-a", "device-b");
        // A 用自己的 id 发标签，B 用「A 的 id」算期望值核对
        assert!(ct_eq(&verify_tag(&k_a, id_a), &verify_tag(&k_b, id_a)));
        // 反方向同理
        assert!(ct_eq(&verify_tag(&k_b, id_b), &verify_tag(&k_a, id_b)));
    }

    /// 配对码不同 → 会话密钥不同 → 交叉核对必须失败。
    #[test]
    fn verify_tags_mismatch_with_wrong_code() {
        let init = start_initiator("123456");
        let resp = start_responder("654321");
        let init_msg = init.message.clone();
        let k_a = derive_session_key(&init.finish(&resp.message).unwrap()).unwrap();
        let k_b = derive_session_key(&resp.finish(&init_msg).unwrap()).unwrap();
        assert!(!ct_eq(
            &verify_tag(&k_a, "device-a"),
            &verify_tag(&k_b, "device-a")
        ));
    }

    /// 两端必须从各自算出的会话密钥派生出**同一个** link secret，
    /// 否则重启后一方用 A、另一方用 B，静默重连会永远失败。
    #[test]
    fn link_secret_matches_on_both_sides() {
        let pw = "482913";
        let init = start_initiator(pw);
        let resp = start_responder(pw);
        let init_msg = init.message.clone();
        let k_a = derive_session_key(&init.finish(&resp.message).unwrap()).unwrap();
        let k_b = derive_session_key(&resp.finish(&init_msg).unwrap()).unwrap();

        let link_a = derive_link_secret(&k_a);
        let link_b = derive_link_secret(&k_b);
        assert_eq!(link_a, link_b);
        // 应为 32 字节 HMAC 的十六进制，且不得等于用户输入的配对码
        assert_eq!(link_a.len(), 64);
        assert_ne!(link_a, pw);
    }

    /// link secret 必须与口令确认标签互不相同，避免把校验值当长期凭据外泄。
    #[test]
    fn link_secret_differs_from_verify_tag() {
        let key = [7u8; 32];
        let tag_hex: String = verify_tag(&key, "")
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        assert_ne!(derive_link_secret(&key), tag_hex);
    }

    #[test]
    fn ct_eq_rejects_length_and_content_diff() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    // ---- 文件回环（回声）防御测试 ----
    use crate::clipboard::types::FileMeta as TestFileMeta;
    use crate::device::identity::DeviceIdentity as TestDeviceIdentity;
    use crate::sync::engine::SyncEngine as TestSyncEngine;
    use std::sync::Arc as TestArc;
    use tokio::sync::mpsc as test_mpsc;

    /// 回归测试：本机复制文件后，对端把同一份文件作为 Offer 广播回来（回环），
    /// 接收方必须丢弃该 Offer，不进入本机待拉取列表、也不 emit 给前端。
    /// 否则表现为「本机复制的文件又出现在本机待拉取列表（本机也拉取了）」。
    #[tokio::test]
    async fn local_file_echo_from_peer_is_dropped() {
        let identity = TestDeviceIdentity::load_or_create("echo-test-node").unwrap();
        let engine = TestSyncEngine::new(identity.clone());
        let hub = ConnectionHub::new(TestArc::new(identity), TestArc::new(engine));

        // 1) 模拟本机复制一个真实存在的文件 → 进入 active_offers（不进本机待拉取）
        let tmp = std::env::temp_dir().join("clipsync_echo_test.txt");
        std::fs::write(&tmp, b"hello clipsync").unwrap();
        hub.offer_local_files(vec![tmp.clone()]);
        assert!(
            !hub.active_offers.lock().unwrap().is_empty(),
            "本机复制后 active_offers 应非空"
        );
        assert!(hub.pending_offers.lock().unwrap().is_empty());

        // 2) 构造「对端回环 Offer」：device_id 是别的设备、文件指纹与本机一致
        let size = std::fs::metadata(&tmp).unwrap().len();
        let echo_files = vec![TestFileMeta {
            file_name: tmp.file_name().unwrap().to_string_lossy().to_string(),
            file_size: size,
            is_dir: false,
            relative_path: "clipsync_echo_test.txt".to_string(),
            modified_at: 0,
            mime_type: String::new(),
            hash: None,
        }];
        let echo_offer = FileFrame::Offer {
            transfer_id: "echo-transfer-1".to_string(),
            device_id: "fake-peer-not-self".to_string(),
            device_name: "WindowsPC".to_string(),
            files: echo_files,
            top_names: vec![],
            has_folder: false,
        };
        let (tx, _rx) = test_mpsc::unbounded_channel::<Outgoing>();

        // 3) 处理该回环 Offer
        let h = TestArc::clone(&hub);
        h.handle_file_frame(echo_offer, &tx, "fake-peer-not-self").await;

        // 4) 断言：本机待拉取列表仍为空（回声被丢弃）
        assert!(
            hub.pending_offers.lock().unwrap().is_empty(),
            "回环 Offer 不应进入本机待拉取列表"
        );

        let _ = std::fs::remove_file(&tmp);
    }

    /// 反向保证：真实的、文件指纹不在本机 active_offers 的对端 Offer 必须正常进入待拉取列表。
    #[tokio::test]
    async fn genuine_remote_offer_is_accepted() {
        let identity = TestDeviceIdentity::load_or_create("echo-test-node-2").unwrap();
        let engine = TestSyncEngine::new(identity.clone());
        let hub = ConnectionHub::new(TestArc::new(identity), TestArc::new(engine));

        let remote_files = vec![TestFileMeta {
            file_name: "photo.png".to_string(),
            file_size: 9999,
            is_dir: false,
            relative_path: "photo.png".to_string(),
            modified_at: 0,
            mime_type: String::new(),
            hash: None,
        }];
        let offer = FileFrame::Offer {
            transfer_id: "remote-transfer-1".to_string(),
            device_id: "genuine-peer".to_string(),
            device_name: "WindowsPC".to_string(),
            files: remote_files,
            top_names: vec![],
            has_folder: false,
        };
        let (tx, _rx) = test_mpsc::unbounded_channel::<Outgoing>();
        let h = TestArc::clone(&hub);
        h.handle_file_frame(offer, &tx, "genuine-peer").await;

        assert_eq!(
            hub.pending_offers.lock().unwrap().len(),
            1,
            "真实对端 Offer 应进入待拉取列表"
        );
    }

    #[test]
    fn offer_fingerprint_ignores_relative_path_and_size() {
        let a = TestFileMeta {
            file_name: "doc.txt".into(),
            file_size: 1234,
            is_dir: false,
            relative_path: "doc.txt".into(),
            modified_at: 0,
            mime_type: String::new(),
            hash: None,
        };
        let mut b = a.clone();
        b.relative_path = "nested/sub/doc.txt".into(); // 不同 relative_path
        assert_eq!(
            ConnectionHub::offer_fingerprint(std::slice::from_ref(&a)),
            ConnectionHub::offer_fingerprint(std::slice::from_ref(&b))
        );

        let mut c = a.clone();
        c.file_size = 9999; // 不同大小
        assert_ne!(
            ConnectionHub::offer_fingerprint(&[a]),
            ConnectionHub::offer_fingerprint(&[c])
        );
    }
}
