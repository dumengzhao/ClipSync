//! 实时日志窗口（log-viewer）后端
//!
//! 方案 D：文件 tail 追踪。窗口打开时启动一个 tokio tail 任务，
//! 定时增量读当天 tracing_appender 滚动日志文件（`clipsync.log.YYYY-MM-DD`），
//! 把新行 emit 给日志窗口；窗口关闭即任务退出，不留任何常驻状态。
//!
//! 生命周期：
//! - `open_log_window` 命令 → 创建/聚焦窗口 → spawn tail 任务；
//! - tail 任务每轮循环检查窗口是否存活（`get_webview_window` 返回 None 即退）；
//! - 窗口关闭后任务自动退出，「仅窗口存在才实时刷新」由此保证。

use tauri::{Emitter, Manager};

/// 日志窗口的窗口 label（capability 白名单必须包含它）
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

/// 当前正在写入的日志文件：扫日志目录，取 `clipsync.log*` 中修改时间最新的那个。
///
/// 不按「当天日期」拼文件名——`tracing_appender::rolling::daily` 的日期拆分时区
/// 不可控（UTC vs 本地），跨天瞬间拼错就会读不到文件。按 mtime 取最新，
/// 跨天轮转、文件重建都能自动跟上。
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
/// 已存在 → show + unminimize + focus（用户选定：聚焦而非再开新实例）；
/// 不存在 → 动态创建 + spawn tail 任务。
#[tauri::command]
pub fn open_log_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(existing) = app.get_webview_window(LOG_WINDOW_LABEL) {
        if existing.is_minimized().unwrap_or(false) {
            let _ = existing.unminimize();
        }
        let _ = existing.show();
        let _ = existing.set_focus();
        return Ok(());
    }

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
    .map_err(|e| format!("创建日志窗口失败: {e}"))?;
    let _ = window.set_focus();
    tracing::info!("日志窗口已打开：启动日志文件 tail（窗口关闭后自动停止）");

    // tail 任务：窗口存活期间增量推送日志行
    spawn_tail_task(app.clone());
    Ok(())
}

/// 启动文件 tail 任务。窗口销毁后自动退出。
fn spawn_tail_task(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        // 当前跟踪的文件路径与已读偏移
        let mut cur_path: Option<std::path::PathBuf> = None;
        let mut offset: u64 = 0;
        // UTF-8 多字节字符/未结束行被截断在读取边界的残段，留待下次拼接
        let mut partial: Vec<u8> = Vec::new();

        loop {
            // 窗口没了 → 任务退出（「仅窗口存在才刷新」的核心保证）
            if app.get_webview_window(LOG_WINDOW_LABEL).is_none() {
                break;
            }

            // 每轮重取「当前正在写入」的日志文件（跨天轮转时自动切换到新文件）
            let Some(path) = current_log_path().await else {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            };

            if cur_path.as_deref() != Some(path.as_path()) {
                // 换文件（首次打开 / 跨天轮转）：从新文件重新算偏移，残段作废
                cur_path = Some(path.clone());
                offset = 0;
                partial.clear();
            }

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
                        // 首轮（offset==0）只回放尾部历史，不从文件头读
                        let replay = offset == 0;
                        let read_from = if replay {
                            len.saturating_sub(TAIL_HISTORY_BYTES)
                        } else {
                            offset
                        };
                        // 单轮读取上限：落后太多时分多轮追上
                        let read_to = (read_from + MAX_READ_PER_TICK).min(len);
                        if let Ok(buf) = read_range(&path, read_from, read_to).await {
                            let mut data = std::mem::take(&mut partial);
                            data.extend_from_slice(&buf);
                            offset = read_to;

                            // 从文件中间开始读（回放尾部）：开头多半是半行，丢弃
                            let begin = if replay && read_from > 0 {
                                match data.iter().position(|&b| b == b'\n') {
                                    Some(i) => i + 1,
                                    None => data.len(),
                                }
                            } else {
                                0
                            };
                            let body = &data[begin..];
                            // 末尾若没有换行符，说明该行还没写完：留给下一轮
                            let (complete, rest) = match body.iter().rposition(|&b| b == b'\n') {
                                Some(i) => (&body[..=i], &body[i + 1..]),
                                None => (&body[..0], body),
                            };
                            partial = rest.to_vec();
                            if partial.len() > MAX_PARTIAL {
                                partial.clear();
                            }

                            if !complete.is_empty() {
                                let lines = split_lines(complete);
                                for chunk in lines.chunks(MAX_LINES_PER_TICK) {
                                    // 负载为纯字符串数组，与前端 `listen<string[]>('log-line')` 对齐
                                    let _ = app.emit_to(LOG_WINDOW_LABEL, "log-line", chunk.to_vec());
                                }
                            }
                        }
                    }
                }
                Err(_) => { /* 文件暂不可读：下轮重试 */ }
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }

        tracing::info!("日志窗口已关闭：停止日志 tail");
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
