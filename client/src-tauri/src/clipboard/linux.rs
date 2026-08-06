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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use arboard::Clipboard;
use async_trait::async_trait;
use parking_lot::Mutex;

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
