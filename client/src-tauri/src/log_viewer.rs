//! 实时日志窗口（log-viewer）后端
//!
//! 方案 D：文件 tail 追踪。窗口打开时启动一个 tokio tail 任务，
//! 增量读 tracing_appender 滚动日志文件（`clipsync.log.YYYY-MM-DD`），
//! 把新行 emit 给日志窗口；窗口关闭即任务退出，不留任何常驻状态。
//!
//! ## 启动时序（为什么不能由 Rust 单方面启动）
//!
//! 窗口刚创建时 WebView 还没执行 JS，Rust 侧此时 emit 的事件会**全部丢失**。
//! 所以历史回放由前端驱动：前端 `await listen(...)` 注册完成后调用
//! [`log_window_ready`]，Rust 在那一刻读文件尾部历史返回给前端，
//! 同时让 tail 任务从**同一偏移**续推增量——
//! 历史（过去）与增量（之后）因此无缝衔接，既不重复也不丢失。
//!
//! 兜底：若前端在 [`READY_TIMEOUT`] 内没有就绪（脚本异常/权限缺失），
//! watchdog 会直接从文件末尾启动 tail，保证窗口至少能实时刷新。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tauri::{Emitter, Manager};

/// 日志窗口的窗口 label。
///
/// ⚠️ 前端 `client/src/main.tsx` 里有一份同样的字符串常量，
/// Tauri 没有跨语言共享机制，**改动必须两处同步**。
pub const LOG_WINDOW_LABEL: &str = "log-viewer";
/// 增量轮询间隔
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);
/// 开窗时回放的历史长度上限（读文件尾部这么多字节）
const TAIL_HISTORY_BYTES: u64 = 200 * 1024;
/// 单次 emit 的行数上限（防异常大文件一次推送过多）
const MAX_LINES_PER_TICK: usize = 500;
/// 单轮最多读取的字节数（防日志暴涨时一次读爆内存）
const MAX_READ_PER_TICK: u64 = 4 * 1024 * 1024;
/// 未结束行残段的最大长度（超过即丢弃，防异常长行撑爆内存）
const MAX_PARTIAL: usize = 1 << 20;
/// 等待前端就绪的兜底时长
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// tail 任务的「代」：每次开新一代（开窗 / 重启 tail）+1。
///
/// **为什么不能只用布尔量**：窗口关闭后旧任务最多 `POLL_INTERVAL`（400ms）才察觉并退出。
/// 若用户在这个窗口期内重新开窗，旧任务会看到「窗口又存在了」而继续跑 —— 于是两个任务同时
/// emit：日志出现重复行、还多跑一份文件 IO；而且其中一个退出时会把运行标记复位，让状态与
/// 事实不符。故任务是**自己那一代**的持有者：代不匹配即立刻退出，且只有仍是当前代时才允许
/// 复位标记。
///
/// 三重作用（与旧的 `TAIL_STARTED` 一致）：
/// 1. 防止「前端就绪命令」与「兜底 watchdog」重复启动同一窗口的 tail；
/// 2. 窗口仍在但任务已异常退出时，`open_log_window` 据此补启动；
/// 3. 任务退出时复位，供下次开窗判断。
static TAIL_GEN: AtomicU64 = AtomicU64::new(0);
/// 「当前代已有 tail 任务在跑」——即旧 `TAIL_STARTED` 的语义，与 `TAIL_GEN` 配对使用。
static TAIL_RUNNING: AtomicBool = AtomicBool::new(false);

/// 抢占启动权：`Some(gen)` = 本次调用负责启动新一代；`None` = 已有任务在跑，不要重复启动。
fn claim_tail_start() -> Option<u64> {
    if TAIL_RUNNING.swap(true, Ordering::SeqCst) {
        None
    } else {
        Some(TAIL_GEN.fetch_add(1, Ordering::SeqCst) + 1)
    }
}

/// 让当前 tail 失效（开新窗口时调用）：旧任务下一轮会发现代不匹配而退出，
/// 同时解除运行标记，允许新一代启动。
fn invalidate_tail() {
    TAIL_GEN.fetch_add(1, Ordering::SeqCst);
    TAIL_RUNNING.store(false, Ordering::SeqCst);
}

/// 当前正在写入的日志文件：扫日志目录，取 `clipsync.log*` 中修改时间最新的那个。
///
/// 不按「当天日期」拼文件名——`tracing_appender::rolling::daily` 的日期拆分时区
/// 不可控（UTC vs 本地），跨天瞬间拼错就会读不到文件。按 mtime 取最新，
/// 跨天轮转、文件重建都能自动跟上。
///
/// 调用方（tail 任务）**按需**调用本函数（首次 + 每约 10s + 读取失败时），
/// 不是每轮 400ms 都扫：整目录列举属于纯重复开销。
async fn current_log_path() -> Option<std::path::PathBuf> {
    let dir = crate::obs::logging::log_dir();
    let mut rd = tokio::fs::read_dir(&dir).await.ok()?;
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("clipsync.log") {
            continue;
        }
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, entry.path()));
        }
    }
    best.map(|(_, p)| p)
}

/// 打开（或聚焦已存在的）实时日志窗口。
///
/// 窗口与主窗口共享同一前端 bundle，前端按 `getCurrentWindow().label === "log-viewer"`
/// 渲染日志组件（与 pull-toast 同机制）。
/// 已存在 → show + unminimize + focus（用户选定：聚焦而非再开新实例），
/// 并顺带检查 tail 是否还活着；不存在 → 动态创建，等前端就绪后启动 tail。
///
/// ## 为什么建窗必须离开主线程（2026-09-22 用户实测的 Windows 死锁）
///
/// `WebviewWindowBuilder::new` 的官方文档在 **Known issues** 里写明：
///
/// > On Windows, this function deadlocks when used in a synchronous command and event
/// > handlers … You should use async commands and separate threads when creating windows.
///
/// 根因：IPC 自定义协议请求由 WebView2 在**主线程**派发，而同步命令是**就地执行**的
/// （`tauri::ipc::private::ResponseTag::block`），于是 `build()` 在主线程里等 WebView2
/// 控制器的创建回调——而那个回调需要主线程继续泵消息循环才能送达。消息循环被自己堵死：
/// 新窗口只剩一个白框、点 X 无反应，托盘与「退出」也全部失灵，只能任务管理器结束进程；
/// 且 `build()` 之后的日志一行都写不出来（这就是故障现场日志里没有「日志窗口已打开」的原因）。
///
/// 因此：**命令标 `async`，并把真正阻塞的建窗放进 `spawn_blocking` 的独立线程**——
/// 正好对应官方给的两条修法（async 命令 + 独立线程）。历史上的「dev 模式建独立窗口
/// 白屏」极可能是同一根因：dev 下页面要等 dev server，创建回调来得更晚，主线程被堵的
/// 概率更高，所以表现为「有时候」。
#[tauri::command]
pub async fn open_log_window(app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || open_log_window_blocking(app))
        .await
        .map_err(|e| format!("日志窗口任务异常终止: {e}"))?
}

/// 供 Rust 侧**主线程回调**（托盘菜单、窗口事件等）使用的入口：把建窗交给独立线程。
///
/// 托盘 `open_logs` 菜单项原先直接调用 `open_log_window`，而菜单回调运行在**主线程**上——
/// 这与「同步命令」是同一个死锁面（官方文档点名的是 "synchronous command **and event
/// handlers**"）。**Rust 侧的调用点一律走这里**，别再直接调阻塞实现。
pub(crate) fn spawn_open_log_window(app: tauri::AppHandle) {
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(e) = open_log_window_blocking(app) {
            tracing::error!("failed to open log window: {e}");
        }
    });
}

/// [`open_log_window`] 的阻塞实现。**只能从非主线程调用**（原因见上方文档注释）。
fn open_log_window_blocking(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(existing) = app.get_webview_window(LOG_WINDOW_LABEL) {
        // 这条分支也要留痕：窗口已存在却白屏时，日志里必须能看出「走的是聚焦分支」
        tracing::info!("日志窗口已存在：聚焦（不重复开窗）");
        if existing.is_minimized().unwrap_or(false) {
            let _ = existing.unminimize();
        }
        let _ = existing.show();
        let _ = existing.set_focus();
        // 窗口还在但任务已退出（异常/被 kill）：补一次启动，否则日志不再刷新。
        if let Some(gen) = claim_tail_start() {
            tracing::warn!(
                gen,
                "日志窗口已存在但 tail 未在运行，重新启动（无历史回放）"
            );
            spawn_tail_task(app.clone(), None, gen);
        }
        return Ok(());
    }

    tracing::info!("正在创建日志窗口（label={LOG_WINDOW_LABEL}）");
    // 窗口重建 → 让上一代任务失效（否则旧任务会继续往新窗口 emit，产生重复行）
    invalidate_tail();
    let window = tauri::WebviewWindowBuilder::new(
        &app,
        LOG_WINDOW_LABEL,
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("ClipSync 日志")
    .inner_size(780.0, 520.0)
    .min_inner_size(520.0, 320.0)
    .resizable(true)
    .decorations(true)
    .build()
    .map_err(|e| {
        let msg = format!("创建日志窗口失败: {e}");
        tracing::error!("{msg}");
        msg
    })?;
    let _ = window.set_focus();
    tracing::info!("日志窗口已打开：等待前端就绪后启动日志 tail");

    // 兜底：前端因故未能发出就绪信号时，也不让窗口永远空白（只是没有历史）。
    // 记住「创建这一刻的代」：若这 3 秒内窗口被关掉又重开（代已变），本兜底必须作废 ——
    // 否则它会替新窗口抢先启动一个「无历史」的 tail，把新窗口的历史回放顶掉。
    let gen_at_creation = TAIL_GEN.load(Ordering::SeqCst);
    let app_fallback = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(READY_TIMEOUT).await;
        if TAIL_GEN.load(Ordering::SeqCst) != gen_at_creation {
            return;
        }
        if let Some(gen) = claim_tail_start() {
            tracing::warn!(
                gen,
                "日志窗口前端 {READY_TIMEOUT:?} 内未就绪，直接启动 tail（无历史回放）"
            );
            spawn_tail_task(app_fallback, None, gen);
        }
    });
    Ok(())
}

/// 前端监听注册完成后调用：返回尾部历史，并从同一位置启动增量 tail。
///
/// 返回的行是「调用这一刻」文件尾部的历史；tail 从读出时的文件末尾偏移续推，
/// 因此两者不会重复也不会留空。重复调用（含兜底已抢先启动）返回空数组——
/// 此时增量推送已经在跑，前端只需等着收事件。
#[tauri::command]
pub async fn log_window_ready(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    let Some(gen) = claim_tail_start() else {
        return Ok(Vec::new());
    };
    let (lines, offset) = read_history().await;
    tracing::info!(
        gen,
        "日志窗口前端就绪：回放 {} 行历史，tail 从偏移 {offset} 续推",
        lines.len()
    );
    spawn_tail_task(app, Some(offset), gen);
    Ok(lines)
}

/// 读日志文件尾部历史，返回（行列表, 续读偏移）。
///
/// 续读偏移指向**最后一个完整行之后**（而不是文件末尾）——文件末尾若有
/// 正在写入的半行，它既不在历史里、也还没到 tail 的起点，会丢；把起点定在
/// 完整行边界上，那半行由 tail 连同后续字节拼成完整行推送。
async fn read_history() -> (Vec<String>, u64) {
    let Some(path) = current_log_path().await else {
        return (Vec::new(), 0);
    };
    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return (Vec::new(), 0);
    };
    let len = meta.len();
    let from = len.saturating_sub(TAIL_HISTORY_BYTES);
    let Ok(buf) = read_range(&path, from, len).await else {
        return (Vec::new(), len);
    };
    // 起点：从文件中段起读时，开头多半是半行，丢弃到第一个换行为止
    let start = if from > 0 {
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => buf.len(),
        }
    } else {
        0
    };
    // 终点：最后一个换行符（含）；其后的半行交给 tail 续读，避免丢失
    let (body, next_offset) = match buf.iter().rposition(|&b| b == b'\n') {
        Some(i) if i + 1 > start => (&buf[start..=i], from + i as u64 + 1),
        _ => (&buf[..0], len),
    };
    (split_lines(body), next_offset)
}

/// 启动文件 tail 任务。窗口销毁后自动退出。
///
/// `initial_offset`：
/// - `Some(off)`：首次读取文件时从 `off` 开始（前端已回放到该处）；
/// - `None`：首次从文件**当前末尾**开始（无历史回放，兜底/重启路径）；
/// - 跨天轮转到的**新文件**一律从头读（新文件内容少且不会与历史重复）。
fn spawn_tail_task(app: tauri::AppHandle, mut initial_offset: Option<u64>, gen: u64) {
    tauri::async_runtime::spawn(async move {
        let mut cur_path: Option<std::path::PathBuf> = None;
        let mut offset: u64 = 0;
        // UTF-8 多字节字符/未结束行被截断在读取边界的残段，留待下次拼接
        let mut partial: Vec<u8> = Vec::new();
        let mut first_file = true;
        // 「当前正在写入哪个文件」的重扫节流计数（见下方注释）
        const RESCAN_TICKS: u32 = 25; // 25 × 400ms = 10s
        let mut ticks_since_scan: u32 = 0;

        loop {
            // 退出条件（两者缺一不可）：
            // 1) 窗口没了 → 正常关闭（「仅窗口存在才刷新」的核心保证）；
            // 2) 代不匹配 → 已被新一代取代（窗口被关掉又重开），旧任务必须让位，
            //    否则会与新任务同时 emit，日志出现重复行并多跑一份 IO。
            if TAIL_GEN.load(Ordering::SeqCst) != gen
                || app.get_webview_window(LOG_WINDOW_LABEL).is_none()
            {
                break;
            }

            // 「当前正在写入」的日志文件**按需重扫**，不是每轮（400ms）都扫。
            // 日志目录里是按天滚动的 7 个文件，每轮 read_dir + 逐条 metadata 属于
            // 纯重复开销（实测日志窗口开着就一直在做无用功）。策略：
            //   - 首次必扫；
            //   - 其后每 RESCAN_TICKS 轮（≈10s）扫一次，跨天轮转最多晚 10s 跟随
            //     （新文件内容少，晚读不会丢数据）；
            //   - 当前文件读不到时（被清理/轮转）立刻置空 → 下一轮强制重扫。
            if cur_path.is_none() || ticks_since_scan >= RESCAN_TICKS {
                ticks_since_scan = 0;
                let Some(found) = current_log_path().await else {
                    tokio::time::sleep(POLL_INTERVAL).await;
                    continue;
                };
                if cur_path.as_deref() != Some(found.as_path()) {
                    cur_path = Some(found.clone());
                    partial.clear();
                    offset = if first_file {
                        first_file = false;
                        match initial_offset.take() {
                            Some(off) => off,
                            None => tokio::fs::metadata(&found)
                                .await
                                .map(|m| m.len())
                                .unwrap_or(0),
                        }
                    } else {
                        // 跨天轮转：新文件从头读
                        0
                    };
                }
            }
            ticks_since_scan = ticks_since_scan.saturating_add(1);
            let Some(path) = cur_path.clone() else {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            };

            match tokio::fs::metadata(&path).await {
                Ok(meta) => {
                    let len = meta.len();
                    if len < offset {
                        // 文件被重建/清空：重置并让前端清屏
                        offset = 0;
                        partial.clear();
                        let _ = app.emit_to(LOG_WINDOW_LABEL, "log-reset", ());
                    }
                    if len > offset {
                        // 单轮读取上限：落后太多时分多轮追上
                        let read_to = (offset + MAX_READ_PER_TICK).min(len);
                        if let Ok(buf) = read_range(&path, offset, read_to).await {
                            let mut data = std::mem::take(&mut partial);
                            data.extend_from_slice(&buf);
                            offset = read_to;
                            // 末尾若没有换行符，说明该行还没写完：留给下一轮
                            let (complete, rest) = match data.iter().rposition(|&b| b == b'\n') {
                                Some(i) => (&data[..=i], &data[i + 1..]),
                                None => (&data[..0], &data[..]),
                            };
                            partial = rest.to_vec();
                            if partial.len() > MAX_PARTIAL {
                                partial.clear();
                            }
                            if !complete.is_empty() {
                                let lines = split_lines(complete);
                                for chunk in lines.chunks(MAX_LINES_PER_TICK) {
                                    // 负载为纯字符串数组，与前端 `listen<string[]>('log-line')` 对齐
                                    let _ =
                                        app.emit_to(LOG_WINDOW_LABEL, "log-line", chunk.to_vec());
                                }
                            }
                        }
                    }
                }
                // 文件暂不可读：置空以便下一轮强制重扫（可能已轮转/被清理）
                Err(_) => cur_path = None,
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }

        // 只有自己仍是当前代时才复位 —— 否则会把新一代的运行标记误抹掉，
        // 导致「窗口仍在但标记为 false」→ 下次开窗重复启动一个 tail。
        if TAIL_GEN.load(Ordering::SeqCst) == gen {
            TAIL_RUNNING.store(false, Ordering::SeqCst);
        }
        tracing::info!(gen, "日志 tail 任务退出（窗口关闭或被新一代取代）");
    });
}

/// 读取文件 [from, to) 字节区间。
async fn read_range(path: &std::path::Path, from: u64, to: u64) -> std::io::Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(std::io::SeekFrom::Start(from)).await?;
    let mut buf = vec![0u8; (to - from) as usize];
    f.read_exact(&mut buf).await?;
    Ok(buf)
}

/// 按行拆分（处理 \n 与 \r\n），返回不含行尾的字符串列表。输入保证以 \n 结尾或为空。
fn split_lines(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for i in 0..data.len() {
        if data[i] == b'\n' {
            let mut end = i;
            if end > start && data[end - 1] == b'\r' {
                end -= 1;
            }
            out.push(String::from_utf8_lossy(&data[start..end]).into_owned());
            start = i + 1;
        }
    }
    out
}
