//! ClipSync - 跨平台剪贴板同步工具
//!
//! 模块结构见 docs/development-plan.md 第十章

pub mod cache;
pub mod clipboard;
pub mod config;
pub mod crypto;
pub mod device;
pub mod discovery;
pub mod error;
pub mod file_server;
pub mod file_share;
pub mod log_viewer;
pub mod obs;
pub mod outbox;
pub mod server_conn;
pub mod sync;
pub mod tauri_cmd;
pub mod transfer;
pub mod update;

use tauri::{
    menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Listener, Manager, WindowEvent,
};

use tauri_plugin_autostart::ManagerExt;

use crate::cache::file_cache::FileCache;
use crate::clipboard::types::DeviceId;
use crate::config::AppConfig;
use crate::device::identity::DeviceIdentity;
use crate::device::registry::DeviceRegistry;
use crate::discovery::manual::ManualAddressBook;
use crate::discovery::{DiscoveredPeer, MdnsDiscovery};
use crate::file_share::FileShare;
use crate::server_conn::{CrossLanOffer, ServerConn};
use crate::sync::engine::SyncEngine;
use crate::transfer::manager::ConnectionHub;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tauri::image::Image;

/// 全局应用状态（在 `setup` 之前通过 `manage` 注入）
pub struct AppState {
    pub identity: DeviceIdentity,
    /// 运行期可改的配置（端口等），背后加锁以支持 `set_config` 热更新
    pub config: Mutex<AppConfig>,
    pub engine: Arc<SyncEngine>,
    /// 传输连接中枢：监听 / 连接对端 / SPAKE2 配对 / 加密通道转发
    pub hub: Arc<ConnectionHub>,
    pub registry: Mutex<DeviceRegistry>,
    pub manual: Mutex<ManualAddressBook>,
    pub cache: FileCache,
    /// 局域网发现控制器（mDNS）；feature 关闭时为无操作占位
    pub discovery: MdnsDiscovery,
    /// 当前通过 mDNS 发现的局域网对端（供 `list_discovered_peers` 查询）
    pub discovered: Mutex<HashMap<String, DiscoveredPeer>>,
    /// 跨局域网文件共享注册表（hash → 本地路径，供内嵌 HTTP 服务拉取）
    pub file_share: Arc<FileShare>,
    /// 跨局域网服务端连接（含已启用节点表 + 启用态）；setup 时建立
    pub server_conn: Mutex<Option<Arc<ServerConn>>>,
    /// 跨 LAN 待复制清单（前端初始化快照用，实时更新走 `cross-lan-file` 事件）
    pub cross_lan_offers: Mutex<Vec<CrossLanOffer>>,
    /// 跨 LAN 文件传输密钥（服务端 Welcome 派生，与文字中继共用 network_key）。
    /// 供内嵌 HTTP 文件服务加密、拉取端解密；未连服务端时为 None。
    pub network_key: Arc<std::sync::Mutex<Option<[u8; 32]>>>,
    /// 本次会话「已下载并校验通过、待安装」的更新包：(绝对路径, sha256)。
    ///
    /// `install_update` **只接受这里记录的路径**：命令参数由前端传入，若不做绑定，
    /// 渲染器一旦被注入（或前端逻辑出错）就能用 `install_update(path=任意 exe)`
    /// 拉起任意程序并让本进程退出——那是等价于代码执行的越权入口。
    pub pending_update: Mutex<Option<(std::path::PathBuf, String)>>,
}

/// 全局上限的**纯选择逻辑**（与副作用解耦，便于单测）：在「局域网待拉取 + 跨 LAN 待复制」
/// 两份清单里，按到达时间保留最新的 `max` 条，返回**应当丢弃**的
/// （待拉取 transfer_id 列表, 跨 LAN 下标列表）。
///
/// 两份清单共用一个额度：不区分来源，谁更旧谁先被丢。
fn select_over_cap(
    pending: &[(String, u64)],
    cross: &[(usize, u64)],
    max: usize,
) -> (Vec<String>, Vec<usize>) {
    enum Src {
        Pending(String),
        Cross(usize),
    }
    let mut all: Vec<(u64, Src)> = pending
        .iter()
        .map(|(id, at)| (*at, Src::Pending(id.clone())))
        .chain(cross.iter().map(|(i, at)| (*at, Src::Cross(*i))))
        .collect();
    if all.len() <= max {
        return (Vec::new(), Vec::new());
    }
    all.sort_by_key(|(at, _)| std::cmp::Reverse(*at)); // 新 → 旧
    let mut drop_pending = Vec::new();
    let mut drop_cross = Vec::new();
    for (_, src) in &all[max..] {
        match src {
            Src::Pending(id) => drop_pending.push(id.clone()),
            Src::Cross(i) => drop_cross.push(*i),
        }
    }
    drop_cross.sort_unstable_by(|a, b| b.cmp(a));
    (drop_pending, drop_cross)
}

/// 「接收到的清单」的**全局上限**：局域网待拉取 + 跨 LAN 待复制，**合计**最多这么多条。
///
/// 用户明确要求：任何地方都不超过 3 条，超出的全部丢弃（不是隐藏、不是延后显示）。
/// 裁剪只在这一个地方做（见 `AppState::enforce_received_cap`）——两份清单分属
/// `hub.pending_offers` 与 `cross_lan_offers`，只有这里能同时看到它们；前端各自裁剪
/// 会出现「前端丢了、后端还在」，重启后又冒出来。
pub const MAX_RECEIVED_OFFERS: usize = 3;

/// 跨 LAN「待复制」清单的留存上限（**只保留最新 N 条**）。
///
/// 每有一个对端复制文件就多一条通知，历史上这里是无上限的 `Vec::push`，长期运行会
/// 无界增长（每条含完整文件清单，不是纯计数）。取值与局域网侧 `MAX_PENDING_OFFERS`
/// 对齐：主界面本来也只显示 3 条（合计），更早的条目用户既看不到也用不上。
const MAX_CROSS_LAN_OFFERS: usize = 3;

impl AppState {
    /// 把两份「接收到的清单」裁剪到全局上限 `MAX_RECEIVED_OFFERS`，并推一份完整快照。
    ///
    /// 语义：**合计最多 N 条**，超出的按到达时间丢弃最旧的（局域网与跨 LAN 一起排队，
    /// 不看来源）。任何一次新增/删除后都应调用一次（同时也是唯一推快照的地方）。
    pub fn enforce_received_cap(&self) {
        let pending = self.hub.pending_offer_ages();
        let cross: Vec<(usize, u64)> = {
            let g = self.cross_lan_offers.lock();
            g.iter()
                .enumerate()
                .map(|(i, o)| (i, o.received_at))
                .collect()
        };
        let (drop_pending, drop_cross) = select_over_cap(&pending, &cross, MAX_RECEIVED_OFFERS);

        if !drop_pending.is_empty() || !drop_cross.is_empty() {
            let n_pending = self.hub.drop_pending_offers(&drop_pending);
            let n_cross = {
                let mut g = self.cross_lan_offers.lock();
                let mut n = 0;
                // 下标从大到小删，避免前面删除导致后面的下标位移
                for i in drop_cross {
                    if i < g.len() {
                        g.remove(i);
                        n += 1;
                    }
                }
                n
            };
            tracing::info!(
                "接收到的清单超过全局上限 {MAX_RECEIVED_OFFERS} 条，已丢弃最旧的：待拉取 {n_pending} 条、跨 LAN {n_cross} 条"
            );
        }
        // 每次调用都推快照：前端以快照为准，不自行累积（也顺带修正去重/顺序）
        self.emit_received_snapshot();
    }

    /// 向两个窗口推送「接收到的清单」完整快照（前端据此整体替换本地列表）。
    pub fn emit_received_snapshot(&self) {
        let Some(app) = self.hub.app_handle() else {
            return; // 运行期才有 AppHandle（启动早期无窗口，无需推）
        };
        let pending = self.hub.pending_offers_snapshot();
        let cross = self.cross_lan_offers.lock().clone();
        let _ = app.emit(
            "received-offers-snapshot",
            serde_json::json!({ "pending": pending, "cross": cross }),
        );
    }

    /// 记录一条跨 LAN「待复制」通知（供前端初始化快照）。**去重 + 上限**：
    ///
    /// - 去重：同一条通知可能被重复投递（服务端重发 / 同一对端重复通知），
    ///   命中既有条目时先移除再追加，视作**最新**（同一份文件在列表里只占一条）。
    /// - 上限：只保留最新 `MAX_CROSS_LAN_OFFERS` 条，超出的从最旧开始淘汰并记 WARN
    ///   （与局域网侧 `track_and_trim` 同语义：静默丢弃会让「列表越来越长」难以察觉）。
    pub fn push_cross_lan_offer(&self, offer: CrossLanOffer) {
        {
            // 单来源上限（防御性；权威上限是 enforce_received_cap 的全局 3 条）
            let mut g = self.cross_lan_offers.lock();
            g.retain(|o| {
                !(o.from == offer.from
                    && o.ext_file_ep == offer.ext_file_ep
                    && o.manifest == offer.manifest)
            });
            g.push(offer);
            while g.len() > MAX_CROSS_LAN_OFFERS {
                let dropped = g.remove(0);
                tracing::warn!(
                    "跨 LAN 待复制清单超过 {MAX_CROSS_LAN_OFFERS} 条上限，已淘汰最早的一条（来自 {}）",
                    crate::obs::logging::log_safe(&dropped.from_name)
                );
            }
        }
        // 锁已释放，再做全局裁剪（enforce 会再锁两份清单，避免同锁重入/交叉加锁）
        self.enforce_received_cap();
    }

    /// 由真实配置构建应用状态。
    ///
    /// identity / engine / hub 由此一次性创建：设备 ID 来自配置中的权威值
    /// （`config.device_id`，首次启动已由 setup 解析并落盘，见 `resolve_device_id`），
    /// 因此本函数必须在 `load_config` 之后调用。
    pub fn build(config: AppConfig) -> Self {
        let identity = DeviceIdentity::new(DeviceId(config.device_id.clone()), &config.device_name)
            .expect("failed to load device identity");
        let engine = Arc::new(SyncEngine::new(identity.clone()));
        let hub = ConnectionHub::new(Arc::new(identity.clone()), engine.clone());

        let mut manual = ManualAddressBook::new();
        for addr in &config.manual_addresses {
            manual.add(addr.clone());
        }

        let cache = FileCache::new(256, config.cache_ttl_hours);

        Self {
            identity,
            config: Mutex::new(config),
            engine,
            hub,
            registry: Mutex::new(DeviceRegistry::new()),
            manual: Mutex::new(manual),
            cache,
            discovery: MdnsDiscovery::new(),
            discovered: Mutex::new(HashMap::new()),
            file_share: Arc::new(FileShare::new()),
            server_conn: Mutex::new(None),
            cross_lan_offers: Mutex::new(Vec::new()),
            network_key: Arc::new(std::sync::Mutex::new(None)),
            pending_update: Mutex::new(None),
        }
    }
}

/// Windows 上 mDNS 入站多播（UDP 5353）默认被防火墙拦截，导致本机收不到对端广播、
/// 局域网发现失效（而出站默认放行，所以本机「能被发现」却「发现不了别人」）。
/// 规则管理策略（业界通行做法，LocalSend 同款交互）：
/// - 安装版：NSIS 安装钩子以管理员权限在 POSTINSTALL 加规则、PREUNINSTALL 删规则；
/// - 绿色版/兜底：设置页「防火墙修复」按钮，用户主动点击触发 UAC 提权执行一次 netsh；
/// - 启动路径**不跑任何 netsh**（曾因每次启动静默跑两趟 netsh 造成闪命令窗 + 无响应）。
#[cfg(windows)]
pub mod firewall {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    pub const RULE_NAME: &str = "ClipSync mDNS (UDP 5353)";

    /// 执行一次 netsh（隐藏窗口）。
    ///
    /// 不在此处 panic：`Command::output()` 在某些环境下会因找不到可执行文件 /
    /// 权限问题直接失败，而这是**用户主动点击**触发的查询路径——把它变成
    /// 崩溃违反本项目「命令层不 panic」的约定（由调用方降级为「规则未生效」并记日志）。
    fn netsh(args: &[&str]) -> std::io::Result<std::process::Output> {
        Command::new("netsh")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    }

    /// 查询放行规则是否已存在（无需管理员权限）。
    /// 查询失败（netsh 起不来 / 无输出）一律按「不存在」处理并记日志，
    /// 让设置页仍可提供「防火墙修复」入口。
    pub fn rule_exists() -> bool {
        let args = [
            "advfirewall",
            "firewall",
            "show",
            "rule",
            &format!("name={RULE_NAME}"),
        ];
        match netsh(&args) {
            Ok(out) => out.status.success(),
            Err(e) => {
                tracing::warn!("执行 netsh 查询防火墙规则失败（按未设置处理）: {e}");
                false
            }
        }
    }

    /// 以管理员权限（触发 UAC）添加放行规则。返回 ()，结果由前端轮询 rule_exists 确认。
    /// 经 powershell Start-Process -Verb runAs 提权执行；用户在 UAC 点「否」时静默失败。
    ///
    /// 坑：Start-Process 的 -ArgumentList 数组模式有老 bug（dotnet/runtime#5576，
    /// 官方不修）——含空格的元素会被拆散，除非手动再包一层双引号。规则名
    /// "name=ClipSync mDNS (UDP 5353)" 含空格，必须整体用 \"...\" 包住，
    /// 否则提权后的 netsh 收到四个碎片参数、静默失败（UAC 通过了规则也没加上）。
    /// 规避：把整条 netsh 命令行作为**单个字符串**传给 -ArgumentList。
    pub fn add_rule_elevated() {
        let netsh_args = format!(
            "advfirewall firewall add rule name=\"{RULE_NAME}\" dir=in action=allow protocol=UDP localport=5353"
        );
        let _ = Command::new("powershell")
            .args([
                "-NoProfile",
                "-WindowStyle",
                "Hidden",
                "-Command",
                // 单字符串 ArgumentList：Start-Process 原样拼接传给 netsh，不再拆分；
                // -Verb runAs 触发 UAC 提权，-Wait 等它执行完
                &format!(
                    "Start-Process netsh -ArgumentList '{}' -Verb runAs -Wait",
                    netsh_args.replace('\'', "''")
                ),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
}

/// 应用入口
pub fn run() {
    crate::obs::logging::init_file_logging();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // 第二个实例启动时不再新建窗口，而是聚焦已运行的第一个实例主窗口
            show_main_window(app);
        }))
        .setup(|app| {
            // 加载持久化配置（覆盖默认），使改过的端口等设置重启后仍生效。
            let handle = app.handle().clone();
            let persisted = {
                let mut cfg = crate::config::load_config(&handle);
                // 配对码：空 / 出厂占位 / **历史低熵格式**（6 位数字等）一律换成新的
                // 高熵码（12 位 base32，60 bit）。旧码熵不足，而配对码是常驻的，
                // 一旦被离线穷举还原即可被长期冒充——不做兼容保留。
                if !crate::crypto::pake::pairing_code_is_current(&cfg.pairing_code) {
                    cfg.pairing_code = crate::crypto::pake::generate_pairing_code();
                }

                // 解析设备 ID（仅首次）：配置已有值则直接沿用；否则取机器码，
                // 机器码也取不到则生成 `000000` 前缀的 fallback 值。解析结果
                // 一律写回配置——config 从此是 device_id 的权威存储，重启直接读。
                {
                    let (id, fresh) = crate::device::identity::resolve_device_id(
                        &cfg.device_id,
                        &crate::device::hardware::hardware_id(),
                    );
                    if cfg.device_id.trim().is_empty() || fresh {
                        if cfg.device_id.trim().is_empty() {
                            tracing::info!("首次解析设备 ID 并写入配置：{id}");
                        } else {
                            tracing::info!("设备 ID 配置为空，重新解析并写入：{id}");
                        }
                        cfg.device_id = id;
                    }
                }

                // 状态注入必须**先于** save_config 等耗时落盘操作：tauri.conf.json 的窗口
                // 先于 setup 闭包创建，webview 加载前端与 setup 并行竞跑，前端首帧就可能
                // 发来 get_config / get_device_id 等命令——manage 晚了这些命令会失败
                // （历史 bug：标题栏设备名空白）。尽早 manage，落盘慢一步无碍。
                app.manage(AppState::build(cfg.clone()));

                if let Err(e) = crate::config::save_config(&handle, &cfg) {
                    // 落盘失败必须让用户看见：尤其 fallback ID（000000 前缀）若没写进
                    // 配置，下次启动会重新生成不同的随机值，身份将不稳定。
                    tracing::error!("启动时写配置失败（device_id/pairing_code 可能未持久化）: {e}");
                }
                cfg
            };

            build_tray(app)?;

            // 对齐「开机自启」与系统自启条目：配置为开则注册、为关则移除，
            // 使重启或状态漂移后自启行为与设置一致（用户在设置页切换也走 set_config 副作用）。
            {
                let mgr = app.autolaunch();
                if let Err(e) = if persisted.auto_start {
                    mgr.enable()
                } else {
                    mgr.disable()
                } {
                    tracing::warn!("autostart 状态对齐失败（不影响启动）: {e}");
                }
            }

            // 主窗口显隐：通用开关「启动后是否打开主窗口」(show_main_window_on_launch)
            // 控制，自启与手动启动行为一致（默认显示）。窗口默认 visible=false，需显式 show。
            {
                if persisted.show_main_window_on_launch {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                    }
                }
            }

            // 应用「默认窗口宽高」配置：用户同时设置了 window_width 与 window_height 时，
            // 覆盖 tauri.conf.json 的默认尺寸；任一为 None（未设置）则沿用当前默认尺寸。
            // 在 show 之后设置也生效，隐藏（--hidden）状态下设置同样有效，之后显示即为该尺寸。
            {
                if let Some(window) = app.get_webview_window("main") {
                    let state = app.state::<AppState>();
                    let (w, h) = {
                        let g = state.config.lock();
                        (g.window_width, g.window_height)
                    };
                    if let (Some(w), Some(h)) = (w, h) {
                        if w == 0 || h == 0 {
                            tracing::warn!("window_width/height 为 0，忽略该尺寸配置");
                        } else if let Err(e) =
                            window.set_size(tauri::LogicalSize::new(w as f64, h as f64))
                        {
                            tracing::warn!("应用默认窗口尺寸失败: {e}");
                        } else {
                            tracing::info!("应用默认窗口尺寸：{}x{}（逻辑像素）", w, h);
                        }
                    } else {
                        // 配置未设置默认宽高：读取「当前窗口实际尺寸」写入 config 落盘作为默认值，
                        // 这样设置页直接读 config 即可，无需每次打开设置时再动态获取当前窗口尺寸。
                        match window.outer_size() {
                            Ok(phys) => {
                                let scale = window.scale_factor().unwrap_or(1.0);
                                let logical = phys.to_logical::<f64>(scale);
                                let dw = logical.width.round() as u32;
                                let dh = logical.height.round() as u32;
                                if dw == 0 || dh == 0 {
                                    tracing::warn!("读取到窗口尺寸为 0，跳过写入默认宽高");
                                } else {
                                    let handle = app.handle().clone();
                                    let cfg = {
                                        let state = app.state::<AppState>();
                                        let mut g = state.config.lock();
                                        g.window_width = Some(dw);
                                        g.window_height = Some(dh);
                                        g.clone()
                                    };
                                    if let Err(e) = crate::config::save_config(&handle, &cfg) {
                                        tracing::warn!("写入默认窗口尺寸失败: {e}");
                                    } else {
                                        tracing::info!(
                                            "配置未设默认宽高，已写入当前窗口尺寸默认值：{}x{}",
                                            dw,
                                            dh
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!("读取当前窗口尺寸失败，跳过写入默认宽高: {e}")
                            }
                        }
                    }
                }
            }

            // 同步「配对码」到连接中枢：作为 SPAKE2 应答方的常驻口令，
            // 对端首配对时输入本机显示的这个码即可（两端无需预先设成相同）。
            {
                let code = app.state::<AppState>().config.lock().pairing_code.clone();
                app.state::<AppState>().hub.set_pairing_code(code);
                let max_folder_files = app.state::<AppState>().config.lock().max_folder_files;
                app.state::<AppState>()
                    .hub
                    .set_max_folder_files(max_folder_files);
            }
            let state = app.state::<AppState>();
            let (enable_mdns, listen_port) = {
                let g = state.config.lock();
                (g.enable_mdns, g.listen_port)
            };
            let identity = state.identity.clone();

            // 恢复已配对设备：设备表从磁盘读，重连口令从密钥链读。必须在传输中枢
            // 启动**之前**装载完，否则监控任务首轮巡检时还认不出这些设备是已配对的。
            {
                let devices = crate::device::store::load_devices(&handle);
                let mut secrets = HashMap::new();
                {
                    let mut reg = state.registry.lock();
                    for d in devices {
                        let id = d.device_id.0.clone();
                        match crate::device::store::load_secret(&handle, &id) {
                            Some(secret) => {
                                secrets.insert(id, secret);
                                reg.add(d);
                            }
                            // 没有口令就无法静默重连。留在表里只会显示成一台永远
                            // 连不上的「已配对」设备，不如剔除，让用户重新配对。
                            None => tracing::warn!(
                                "设备 {} 的配对口令已丢失，需重新配对",
                                d.device_name
                            ),
                        }
                    }
                }
                state.hub.restore_paired(secrets);
            }

            // 启动局域网发现（mDNS 广播本机 + 订阅对端），失败仅记录不阻断启动。
            // 端口来自配置（默认 20071，可改）；发现方从对端广告动态读取端口，不写死。
            if enable_mdns {
                if let Err(e) =
                    app.state::<AppState>()
                        .discovery
                        .start(&handle, &identity, listen_port)
                {
                    tracing::error!("mDNS discovery failed to start: {e}");
                }
                // 防火墙规则不在启动路径处理（历史教训：每次启动静默跑 netsh 造成
                // 闪命令窗 + 数秒无响应，且非管理员必失败从未生效）。安装版由 NSIS
                // 钩子处理；绿色版由设置页「防火墙修复」按钮提权执行。
            }

            // 启动同步引擎（剪贴板监听 + 事件广播），失败仅记录不阻断启动
            let engine = app.state::<AppState>().engine.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = engine.start(handle).await {
                    tracing::error!("sync engine failed to start: {e}");
                }
            });

            // 启动传输中枢（监听 / 连接对端 / SPAKE2 配对 / 加密转发），失败仅记录不阻断启动
            let hub = app.state::<AppState>().hub.clone();
            let hub_app = app.handle().clone();
            let hub_port = listen_port;
            tauri::async_runtime::spawn(async move {
                hub.start(hub_app, hub_port).await;
            });

            // 启动跨局域网服务端连接（常连 + 心跳 + 中继路由）；失败仅记录不阻断启动
            {
                let sc =
                    ServerConn::new(app.handle().clone(), app.state::<AppState>().engine.clone());
                sc.start();
                *app.state::<AppState>().server_conn.lock() = Some(sc);
            }

            // 跨 LAN 文件直取复用上面的 listen_port（由 transfer/manager.rs 的 accept
            // 循环在收到 GET /file/ 时分流），无需在此另起服务。

            // 把跨 LAN 待复制通知同时缓冲进状态，供前端初始化快照
            {
                let app_handle = app.handle().clone();
                let _ = app_handle.clone().listen("cross-lan-file", move |event| {
                    if let Ok(o) = serde_json::from_str::<CrossLanOffer>(event.payload()) {
                        app_handle.state::<AppState>().push_cross_lan_offer(o);
                    }
                });
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            // 仅对主窗口：点关闭按钮（X）隐藏而非退出进程；设置等其它窗口正常关闭
            if window.label() == "main" {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    hide_main_window(window.app_handle());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            tauri_cmd::get_version,
            tauri_cmd::get_device_id,
            tauri_cmd::get_device_name,
            tauri_cmd::get_config,
            tauri_cmd::get_window_size,
            tauri_cmd::get_clipboard,
            tauri_cmd::set_clipboard,
            tauri_cmd::get_paired_devices,
            tauri_cmd::add_manual_address,
            tauri_cmd::list_manual_addresses,
            tauri_cmd::remove_manual_address,
            tauri_cmd::cache_stats,
            tauri_cmd::set_config,
            tauri_cmd::list_discovered_peers,
            tauri_cmd::list_connected_peers,
            tauri_cmd::pair_with,
            tauri_cmd::pair_manual,
            tauri_cmd::regenerate_pairing_code,
            tauri_cmd::unpair,
            tauri_cmd::pull_files,
            tauri_cmd::cancel_pull,
            tauri_cmd::list_pending_offers,
            tauri_cmd::clear_received_offers,
            tauri_cmd::open_settings,
            tauri_cmd::quit_app,
            log_viewer::open_log_window,
            log_viewer::log_window_ready,
            hide_app_window,
            win_minimize,
            win_toggle_maximize,
            tauri_cmd::get_server_status,
            tauri_cmd::get_server_nodes,
            update::check_update,
            update::download_update,
            update::install_update,
            update::is_installed_build_cmd,
            tauri_cmd::list_cross_lan_offers,
            tauri_cmd::pull_cross_lan,
            tauri_cmd::cancel_pull_cross_lan,
            tauri_cmd::probe_ext_file_ep,
            tauri_cmd::scan_lan_server_configs,
            tauri_cmd::apply_lan_server_config,
            tauri_cmd::clear_network_token,
            tauri_cmd::firewall_rule_exists,
            tauri_cmd::firewall_fix,
            tauri_cmd::show_pull_toast,
            tauri_cmd::hide_pull_toast,
            #[cfg(debug_assertions)]
            tauri_cmd::simulate_incoming_offer,
            #[cfg(debug_assertions)]
            tauri_cmd::debug_report_mount,
            #[cfg(debug_assertions)]
            tauri_cmd::simulate_cross_lan_offer,
            #[cfg(debug_assertions)]
            tauri_cmd::debug_toast_log,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// 构建系统托盘图标与菜单
/// 托盘图标圆点指示：推送文本(左上) / 收到文本(右上)，3 秒后还原。
#[derive(Clone, Copy)]
pub enum TrayDotKind {
    Push,
    Receive,
}

struct TrayDotIcons {
    base: Image<'static>,
    push: Image<'static>,
    receive: Image<'static>,
}

static TRAY_DOT_ICONS: OnceLock<TrayDotIcons> = OnceLock::new();
static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();
static TRAY_DOT_GEN: AtomicU64 = AtomicU64::new(0);

/// 启动时调用：缓存应用句柄与三张托盘图标，返回 base 图标供托盘构建使用。
pub fn init_tray_dot(app: &tauri::App) -> Image<'static> {
    let base = Image::from_bytes(include_bytes!("../icons/tray-icon.png"))
        .expect("tray-icon.png decode failed");
    let push = Image::from_bytes(include_bytes!("../icons/tray-icon-dot-left.png"))
        .expect("tray-icon-dot-left.png decode failed");
    let receive = Image::from_bytes(include_bytes!("../icons/tray-icon-dot-right.png"))
        .expect("tray-icon-dot-right.png decode failed");
    let _ = TRAY_DOT_ICONS.set(TrayDotIcons {
        base: base.clone(),
        push,
        receive,
    });
    let _ = APP_HANDLE.set(app.app_handle().clone());
    base
}

/// 在托盘图标上叠加圆点（推送=左上 / 收到=右上），3 秒后还原基础图标。
/// 3 秒内再次触发会刷新代次，使还原时间顺延到最后一次触发之后，避免来回闪烁。
pub fn tray_dot(kind: TrayDotKind) {
    let (Some(app), Some(icons)) = (APP_HANDLE.get(), TRAY_DOT_ICONS.get()) else {
        return;
    };
    let dot = match kind {
        TrayDotKind::Push => &icons.push,
        TrayDotKind::Receive => &icons.receive,
    };
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_icon(Some(dot.clone()));
    }
    let gen = TRAY_DOT_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if TRAY_DOT_GEN.load(Ordering::SeqCst) == gen {
            if let (Some(tray), Some(icons)) = (app2.tray_by_id("main"), TRAY_DOT_ICONS.get()) {
                let _ = tray.set_icon(Some(icons.base.clone()));
            }
        }
    });
}

fn build_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let show_i = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let hide_i = MenuItem::with_id(app, "hide", "隐藏主窗口", true, None::<&str>)?;
    let settings_i = MenuItem::with_id(app, "settings", "设置", true, None::<&str>)?;
    let logs_i = MenuItem::with_id(app, "open_logs", "查看日志", true, None::<&str>)?;
    // 仅安装版提供「检查更新」：绿色版走更新会把自己悄悄变成安装版，且源码目录那份
    // 不会被替换（多出一份副本）。判定逻辑见 update::is_installed_build。
    let update_i = if crate::update::is_installed_build() {
        Some(MenuItem::with_id(
            app,
            "check_update",
            "检查更新",
            true,
            None::<&str>,
        )?)
    } else {
        tracing::info!("免安装版：托盘不显示「检查更新」（更新仅面向安装版）");
        None
    };
    let sep_i = PredefinedMenuItem::separator(app)?;
    let quit_i = MenuItem::with_id(app, "quit", "退出 ClipSync", true, None::<&str>)?;
    let mut items: Vec<&dyn IsMenuItem<tauri::Wry>> = vec![&show_i, &hide_i, &settings_i, &logs_i];
    if let Some(u) = update_i.as_ref() {
        items.push(u);
    }
    items.push(&sep_i);
    items.push(&quit_i);
    let menu = Menu::with_items(app, &items)?;

    // 托盘专用图标：内嵌编译进二进制，dev/build 均可靠；与窗口应用图标解耦。
    // 同时缓存圆点变体（推送=左上 / 收到=右上）供 tray_dot 切换。
    let tray_icon = init_tray_dot(app);

    TrayIconBuilder::with_id("main")
        .icon(tray_icon)
        .icon_as_template(false)
        .tooltip("ClipSync")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "hide" => hide_main_window(app),
            "settings" => {
                if let Err(e) = crate::tauri_cmd::open_settings(app.clone()) {
                    tracing::error!("failed to open settings window: {e}");
                }
            }
            "open_logs" => {
                if let Err(e) = crate::log_viewer::open_log_window(app.clone()) {
                    tracing::error!("failed to open log window: {e}");
                }
            }
            "check_update" => {
                let app_handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    use tauri_plugin_dialog::DialogExt;
                    let server_url = {
                        let state = app_handle.state::<AppState>();
                        let url = state.config.lock().server_url.clone();
                        url
                    };
                    match crate::update::do_check_update(&server_url).await {
                        Ok(Some(info)) => {
                            tracing::info!("托盘检查更新：发现新版本 {}（当前 {}）", info.version, env!("CARGO_PKG_VERSION"));
                            let notes = if info.notes.trim().is_empty() {
                                "（无更新说明）"
                            } else {
                                info.notes.trim()
                            };
                            let msg = format!(
                                "发现新版本 {}（{}）\n\n{}",
                                info.version, info.pub_date, notes
                            );
                            let (url, sha256) = (info.url.clone(), info.sha256.clone());
                            let app_dl = app_handle.clone();
                            app_handle
                                .dialog()
                                .message(msg)
                                .title("发现新版本")
                                .kind(tauri_plugin_dialog::MessageDialogKind::Info)
                                .buttons(
                                    tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
                                        "下载并安装".to_string(),
                                        "稍后".to_string(),
                                    ),
                                )
                                .show(move |confirmed| {
                                    if !confirmed {
                                        tracing::info!("托盘检查更新：用户取消下载");
                                        return;
                                    }
                                    tracing::info!("托盘检查更新：用户确认下载安装");
                                    tauri::async_runtime::spawn(async move {
                                        use tauri_plugin_dialog::DialogExt;
                                        // 主窗口可能处于隐藏状态：先显示出来，否则下载进度用户完全看不到，
                                        // 会误以为点了「下载并安装」之后程序没反应。
                                        show_main_window(&app_dl);
                                        // 给前端一点时间渲染并注册 update-progress 监听，
                                        // 否则开头几个进度事件会因为监听尚未就绪而丢失。
                                        tokio::time::sleep(std::time::Duration::from_millis(400))
                                            .await;
                                        match crate::update::download_update(
                                            app_dl.clone(),
                                            url,
                                            sha256,
                                        )
                                        .await
                                        {
                                            Ok(path) => {
                                                tracing::info!("托盘更新下载完成（sha256 校验通过）：{path}，启动安装器");
                                                let state = app_dl.state::<AppState>();
                                                if let Err(e) =
                                                    crate::update::install_verified_update(
                                                        &state,
                                                        std::path::Path::new(&path),
                                                    )
                                                    .await
                                                {
                                                    tracing::error!("托盘更新启动安装失败：{e}");
                                                    app_dl
                                                        .dialog()
                                                        .message(format!("启动安装失败：{e}"))
                                                        .title("更新")
                                                        .kind(
                                                            tauri_plugin_dialog::MessageDialogKind::Error,
                                                        )
                                                        .show(|_| {});
                                                }
                                                // Windows 成功路径在 install_update 内
                                                // spawn NSIS 后直接 exit(0)，不会走到这里之后
                                            }
                                            Err(e) => {
                                                tracing::error!("托盘更新下载失败：{e}");
                                                app_dl
                                                    .dialog()
                                                    .message(format!("下载失败：{e}"))
                                                    .title("更新")
                                                    .kind(
                                                        tauri_plugin_dialog::MessageDialogKind::Error,
                                                    )
                                                    .show(|_| {});
                                            }
                                        }
                                    });
                                });
                        }
                        Ok(None) => {
                            tracing::info!("托盘检查更新：已是最新");
                            app_handle
                                .dialog()
                                .message("当前已是最新版本。")
                                .title("检查更新")
                                .show(|_| {});
                        }
                        Err(e) => {
                            tracing::warn!("托盘检查更新失败：{e}");
                            app_handle
                                .dialog()
                                .message(format!("检查更新失败：{e}"))
                                .title("检查更新")
                                .kind(tauri_plugin_dialog::MessageDialogKind::Error)
                                .show(|_| {});
                        }
                    }
                });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event({
            // 跨平台双击检测：tray-icon 的 `DoubleClick` 仅在 Windows 派发（源码注释
            // "Windows Only"），macOS / Linux 只发 `Click`。故统一从 `Click` 手动判定
            // 双击。单次物理点击会发 Down+Up 两个 Click，因此只数 `Left + Up` 避免重复计数。
            let last_left_click = std::sync::Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
            move |tray, event| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    let now = std::time::Instant::now();
                    let mut guard = last_left_click.lock().unwrap();
                    let is_double = guard
                        .is_some_and(|t| now.duration_since(t) <= std::time::Duration::from_millis(500));
                    if is_double {
                        *guard = None;
                        show_main_window(tray.app_handle());
                    } else {
                        *guard = Some(now);
                    }
                }
            }
        })
        .build(app)?;

    // 启动时隐藏 Dock（仅在菜单栏运行）
    #[cfg(target_os = "macos")]
    {
        use tauri::ActivationPolicy;
        let _ = app.set_activation_policy(ActivationPolicy::Accessory);
    }

    Ok(())
}

/// 显示主窗口并恢复 Dock 图标
fn show_main_window(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    {
        use tauri::ActivationPolicy;
        let _ = app.set_activation_policy(ActivationPolicy::Regular);
    }
    if let Some(w) = app.get_webview_window("main") {
        // 同 open_settings：最小化后 show()/set_focus() 无法还原窗口，需先 unminimize()。
        if w.is_minimized().unwrap_or(false) {
            w.unminimize().ok();
        }
        w.show().ok();
        w.set_focus().ok();
        // 通知前端窗口已显示：TitleBar 监听此事件强制回流，清除隐藏期间残留的 :hover 红底
        let _ = app.emit("main-shown", ());
    }
}

/// 隐藏主窗口并移除 Dock 图标（仅菜单栏）
fn hide_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        w.hide().ok();
    }
    #[cfg(target_os = "macos")]
    {
        use tauri::ActivationPolicy;
        let _ = app.set_activation_policy(ActivationPolicy::Accessory);
    }
}

/// 自定义标题栏「关闭」按钮调用：隐藏主窗口而非退出进程（与点击原生 X 行为一致）
#[tauri::command]
fn hide_app_window(app: tauri::AppHandle) {
    hide_main_window(&app);
}

/// 标题栏「最小化」按钮：走 Rust 命令而非 JS `getCurrentWindow().minimize()`，
/// 因为该 JS 窗口写操作在本项目未被授予权限（日志曾报 allow-minimize not allowed），
/// 而 Rust 侧调用无需前端窗口权限。
#[tauri::command]
fn win_minimize(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.minimize();
    }
}

/// 标题栏「最大化/还原」按钮，理由同上（allow-toggle-maximize not allowed）。
/// WebviewWindow 无 toggle_maximize，手动根据当前状态切换。
#[tauri::command]
fn win_toggle_maximize(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_maximized().unwrap_or(false) {
            let _ = w.unmaximize();
        } else {
            let _ = w.maximize();
        }
    }
}

#[cfg(test)]
mod received_cap_tests {
    use super::{select_over_cap, MAX_RECEIVED_OFFERS};

    /// 全局上限：**合计**最多 3 条，两份清单共用一个额度，按到达时间丢弃最旧的。
    /// 用户要求：「仅保留三条就是三条，任何地方都不需要超过三条，超过的全部丢弃」。
    #[test]
    fn global_cap_keeps_newest_three_across_both_lists() {
        // 局域网 2 条（较旧）+ 跨 LAN 2 条（较新）→ 合计 4，应丢弃最旧的那条局域网
        let pending = vec![("p-old".to_string(), 100), ("p-new".to_string(), 400)];
        let cross = vec![(0usize, 200), (1usize, 300)];
        let (drop_p, drop_c) = select_over_cap(&pending, &cross, MAX_RECEIVED_OFFERS);
        assert_eq!(drop_p, vec!["p-old".to_string()]);
        assert!(drop_c.is_empty());

        // 跨 LAN 更旧则丢跨 LAN（下标从大到小返回，便于安全删除）
        // 5 条裁到 3 条 → 丢 2 条：cross 的 100/150 最旧，降序返回 [2, 0]
        let pending = vec![("p1".to_string(), 900), ("p2".to_string(), 800)];
        let cross = vec![(0usize, 100), (1usize, 200), (2usize, 150)];
        let (drop_p, drop_c) = select_over_cap(&pending, &cross, MAX_RECEIVED_OFFERS);
        assert!(drop_p.is_empty());
        assert_eq!(drop_c, vec![2usize, 0usize]);

        // 未超限：什么都不丢
        let (drop_p, drop_c) =
            select_over_cap(&[("a".to_string(), 1)], &[(0usize, 2)], MAX_RECEIVED_OFFERS);
        assert!(drop_p.is_empty() && drop_c.is_empty());

        // 极端：全部来自同一来源也会裁到 3 条
        // 返回的是条目 id（按 key 删除，删除顺序无关），故按集合比较、
        // 不断言迭代顺序——只有 cross 的下标需要降序（见上方注释）。
        let many: Vec<(String, u64)> = (0..5u64).map(|i| (format!("p{i}"), i)).collect();
        let (drop_p, _) = select_over_cap(&many, &[], MAX_RECEIVED_OFFERS);
        assert_eq!(drop_p.len(), 2, "5 条只留 3 条，丢 2 条");
        let mut dropped = drop_p;
        dropped.sort();
        assert_eq!(dropped, vec!["p0".to_string(), "p1".to_string()]);
    }
}
