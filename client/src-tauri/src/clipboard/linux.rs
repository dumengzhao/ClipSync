//! Linux 剪贴板实现（X11 + Wayland，基于 arboard）
//!
//! 设计要点：
//! - 用一个**进程级常驻**的 `arboard::Clipboard` 句柄（`inner`）做所有读写。X11 / Wayland
//!   是「所有权」模型：写入方必须作为 selection owner 常驻，持续应答其它进程的
//!   SelectionRequest。`engine.rs` 已经保证只创建一次 `PlatformClipboard` 并全程复用——
//!   这里不再 `new()` 后立即丢弃，否则 `apply_remote` 写入后所有权瞬间丢失，对端粘贴拿到空内容
//!   （即 `fbd3764` 修掉的「单向不同步」根因）。
//! - `watch` 用**另一个独立**的 arboard 实例做轮询（每 ~250ms 比较文本），检测本地剪贴板变化后
//!   回调 `cb()`。arboard 3.6 已无「等待变化」的阻塞 API（仅剩 `SetExtLinux::wait`，语义是
//!   「block 直到我们写入的内容被替换」，并非监听变化），且 X11 / Wayland 行为不一；文本轮询是
//!   两种会话下都稳的方案。由于图片跨端线格式尚未统一（见下），这里只轮询文本。
//!
//! 已知限制（与 Windows 侧对齐后再解）：
//! - 图片跨端：Windows 写的是 CF_DIB 原始字节，本实现 `read` 出来的是 arboard 的 RGBA，二者
//!   线格式不一致，跨平台图片同步仍不可用；文本完全可用。统一成 PNG 线格式是独立的后续任务。
//! - `write_delayed_files`（文件同步）属于阶段五，尚未实现，与 Windows 侧一致返回未实现。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use arboard::Clipboard;
use async_trait::async_trait;
use parking_lot::Mutex;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt;

use super::types::{ClipboardContent, FileMeta, SyncId, WatchHandle};
use super::ClipboardProvider;

/// 进程级唯一的剪贴板句柄。`Option` 是为让 `new()` 在拿不到显示会话时不致命失败。
pub struct LinuxClipboard {
    inner: Arc<Mutex<Option<Clipboard>>>,
}

impl LinuxClipboard {
    pub fn new() -> Self {
        let inner = match Clipboard::new() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::error!("初始化 Linux 剪贴板失败（可能无图形会话）: {e}");
                None
            }
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }
}

#[async_trait]
impl ClipboardProvider for LinuxClipboard {
    async fn read(&self) -> Result<ClipboardContent> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.lock();
            let cb = match guard.as_mut() {
                Some(cb) => cb,
                None => anyhow::bail!("剪贴板不可用（无图形会话？）"),
            };
            // 先试文本：成功即返回。非文本选择会让 get_text 返回 Err，自然落到图片分支。
            if let Ok(text) = cb.get_text() {
                return Ok(ClipboardContent::Text(text));
            }
            // 再试图片（arboard 给的是 RGBA 字节，Cow 需转 Vec）。
            if let Ok(img) = cb.get_image() {
                return Ok(ClipboardContent::Image {
                    data: img.bytes.to_vec(),
                    max_size: super::types::DEFAULT_MAX_IMAGE_SIZE,
                });
            }
            anyhow::bail!("剪贴板中无可识别的格式")
        })
        .await
        .map_err(|e| anyhow::anyhow!("剪贴板读取任务失败: {e}"))?
    }

    async fn write(&self, content: ClipboardContent) -> Result<()> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.lock();
            let cb = match guard.as_mut() {
                Some(cb) => cb,
                None => anyhow::bail!("剪贴板不可用（无图形会话？）"),
            };
            match content {
                ClipboardContent::Text(text) => cb.set_text(text)?,
                // 跨平台图片线格式未统一（见文件头注释），写入对端无法正确重建，先明确报错。
                ClipboardContent::Image { .. } => anyhow::bail!(
                    "跨平台图片同步尚未统一线格式（Windows=CF_DIB / Linux=RGBA），文本同步已可用"
                ),
                other => anyhow::bail!("不支持写入的剪贴板内容: {other:?}"),
            }
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("剪贴板写入任务失败: {e}"))?
    }

    async fn write_delayed_files<F>(
        &self,
        _files: Vec<FileMeta>,
        _sync_id: SyncId,
        _fetch_cb: F,
    ) -> Result<()>
    where
        F: Fn(usize, u64, u32) -> Result<Vec<u8>> + Send + Sync + 'static,
    {
        anyhow::bail!("延迟渲染文件写入尚未实现（阶段五）")
    }

    async fn read_file_paths(&self) -> Result<Vec<PathBuf>> {
        // 文件剪贴板走 X11 selection（text/uri-list 或 gnome-copied-files），arboard 不暴露此 API。
        read_clipboard_files_x11()
    }

    async fn write_file_paths(&self, paths: &[PathBuf]) -> Result<()> {
        // 拉取完成后自动写剪贴板：接管 CLIPBOARD selection 并应答 SelectionRequest，
        // 提供 text/uri-list，使用户在文件管理器中直接 Ctrl+V 即可粘贴。
        write_clipboard_files_x11(paths)
    }

    async fn watch(&self, cb: Box<dyn Fn() + Send>) -> Result<WatchHandle> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        thread::spawn(move || {
            let mut watcher = match Clipboard::new() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("无法初始化 Linux 剪贴板监听: {e}");
                    return;
                }
            };
            // 以当前内容作为基线，避免启动时把已有剪贴板误报为「一次变化」。
            let mut last_text = watcher.get_text().unwrap_or_default();
            while !stop_clone.load(Ordering::SeqCst) {
                if let Ok(t) = watcher.get_text() {
                    if t != last_text {
                        last_text = t.clone();
                        cb();
                    }
                }
                thread::sleep(Duration::from_millis(250));
            }
        });
        Ok(WatchHandle::new(move || stop.store(true, Ordering::SeqCst)))
    }

    async fn has_sync_mark(&self, _sync_id: &SyncId) -> Result<bool> {
        // 与 Windows / macOS 侧一致：阶段三嵌入自定义格式后再实现防回环判定。
        Ok(false)
    }
}

// ===========================================================================
// X11 文件剪贴板（x11rb）—— arboard 不暴露文件 API，需直接操作 selection。
//
// 说明：此段仅 Linux 编译，无法在 Windows 主机验证；正确性依赖 x11rb 0.13 API
// 与 Ubuntu 对端运行时联调。read 用于「探测本地文件拷贝」，write 用于「拉取后
// 自动粘贴」（接管 CLIPBOARD 所有权并应答 SelectionRequest）。
// ===========================================================================

fn intern<C: Connection>(conn: &C, name: &[u8]) -> Result<u32> {
    Ok(conn.intern_atom(false, name)?.reply()?.atom)
}

/// 读取本机 CLIPBOARD 中的文件 URI 列表（优先 gnome-copied-files，回退 text/uri-list）。
fn read_clipboard_files_x11() -> Result<Vec<PathBuf>> {
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::WindowClass;

    let (conn, screen) = x11rb::connect(None).map_err(|e| anyhow!("{e}"))?;
    let root = conn.setup().roots[screen].root;

    let clipboard_atom = intern(&conn, b"CLIPBOARD")?;
    let owner = conn.get_selection_owner(clipboard_atom)?.reply()?.owner;
    if owner == x11rb::NONE {
        return Ok(Vec::new());
    }

    let gnome_atom = intern(&conn, b"x-special/gnome-copied-files")?;
    let uri_atom = intern(&conn, b"text/uri-list")?;
    let prop = intern(&conn, b"CLIPSYNC_FILE_PROP")?;

    let req_win = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT as u8,
        req_win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &Default::default(),
    )?;

    // 依次尝试两种 mime；任一成功即解析返回。
    for target in [gnome_atom, uri_atom] {
        conn.convert_selection(req_win, clipboard_atom, target, prop, x11rb::CURRENT_TIME)?;
        conn.flush()?;
        let mut raw = Vec::new();
        let mut got = false;
        while let Ok(event) = conn.wait_for_event() {
            if let Event::SelectionNotify(n) = event {
                if n.selection == clipboard_atom && n.property != x11rb::NONE {
                    raw = read_property(&conn, req_win, prop)?;
                    got = true;
                }
                break;
            }
        }
        if got {
            // gnome-copied-files 首行为 "copy"/"cut"，需跳过
            return Ok(parse_uri_list(&raw));
        }
    }
    Ok(Vec::new())
}

/// 读取属性全部数据。MVP：不处理 >256KB 的 INCR 分片（文件清单通常很小）。
fn read_property<C: Connection>(conn: &C, window: u32, property: u32) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let reply = conn
        .get_property(false, window, property, x11rb::NONE, 0, 256 * 1024)?
        .reply()?;
    data.extend_from_slice(&reply.value);
    Ok(data)
}

/// 把一组本地路径写回 CLIPBOARD（接管所有权，应答 SelectionRequest 提供 text/uri-list）。
fn write_clipboard_files_x11(paths: &[PathBuf]) -> Result<()> {
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{EventMask, PropMode, SelectionNotifyEvent, WindowClass};

    let uris = paths_to_uri_list(paths);
    let (conn, screen) = x11rb::connect(None).map_err(|e| anyhow!("{e}"))?;
    let root = conn.setup().roots[screen].root;

    let owner_win = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT as u8,
        owner_win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &Default::default(),
    )?;
    let clipboard_atom = intern(&conn, b"CLIPBOARD")?;
    let uri_atom = intern(&conn, b"text/uri-list")?;
    let targets_atom = intern(&conn, b"TARGETS")?;
    let atom_atom = intern(&conn, b"ATOM")?;
    let prop_atom = intern(&conn, b"CLIPSYNC_FILE_PROP")?;

    conn.set_selection_owner(owner_win, clipboard_atom, x11rb::CURRENT_TIME)?;
    conn.flush()?;

    let conn = std::sync::Arc::new(conn);
    let data = Arc::new(uris);
    thread::spawn(move || {
        loop {
            let event = match conn.wait_for_event() {
                Ok(e) => e,
                Err(_) => break,
            };
            match event {
                Event::SelectionRequest(req) => {
                    if req.selection != clipboard_atom {
                        continue;
                    }
                    let is_uri = req.target == uri_atom;
                    let is_targets = req.target == targets_atom;
                    if is_targets {
                        let targets: Vec<u32> = vec![uri_atom];
                        let bytes: Vec<u8> =
                            targets.iter().flat_map(|a| a.to_ne_bytes()).collect();
                        let _ = conn.change_property(
                            PropMode::REPLACE,
                            req.requestor,
                            req.property,
                            atom_atom,
                            32,
                            targets.len() as u32,
                            &bytes,
                        );
                    } else if is_uri {
                        let _ = conn.change_property(
                            PropMode::REPLACE,
                            req.requestor,
                            req.property,
                            uri_atom,
                            8,
                            data.as_slice().len() as u32,
                            data.as_slice(),
                        );
                    }
                    let notify = SelectionNotifyEvent {
                        response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: req.time,
                        requestor: req.requestor,
                        selection: req.selection,
                        target: req.target,
                        property: if is_uri || is_targets {
                            req.property
                        } else {
                            x11rb::NONE
                        },
                    };
                    let notify_bytes: [u8; 32] = notify.into();
                    let _ = conn.send_event(
                        false,
                        req.requestor,
                        EventMask::NO_EVENT,
                        notify_bytes,
                    );
                    let _ = conn.flush();
                }
                Event::SelectionClear(_) => break,
                _ => {}
            }
        }
    });
    Ok(())
}

/// 把路径列表编码为 text/uri-list 字节（每行一个 file:// URI）。
fn paths_to_uri_list(paths: &[PathBuf]) -> Vec<u8> {
    let mut s = String::new();
    for p in paths {
        s.push_str("file://");
        s.push_str(&p.to_string_lossy());
        s.push('\n');
    }
    s.into_bytes()
}

/// 解析 text/uri-list / gnome-copied-files 字节为绝对路径列表。
fn parse_uri_list(raw: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(raw);
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // gnome-copied-files 首行为 "copy"/"cut"，不是 URI，跳过看起来不像 URI 的行
        if let Some(path) = uri_to_path(line) {
            out.push(path);
        }
    }
    out
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let path = uri.strip_prefix("file://")?;
    Some(PathBuf::from(percent_decode(path)))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hexval(bytes[i + 1]), hexval(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
