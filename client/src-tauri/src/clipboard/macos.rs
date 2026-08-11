//! macOS 剪贴板实现
//!
//! 文本/图片读写与文件列表读写均基于 arboard（其内部分封装 NSPasteboard
//! generalPasteboard，已显式声明 Send/Sync）。文件列表读用 `get().file_list()`
//! 读 NSFilenamesPboardType；写用 `set().file_list()` 经 NSURL + writeObjects 落盘。
//!
//! 监听（`watch`）：macOS 没有可靠的原生「剪贴板变化」通知，业界（Paste、Maccy 等）
//! 普遍做法是轮询 `NSPasteboard.changeCount` 这个自增整数来判断变化——只比较整数、
//! 不读取剪贴板内容，比在前端每秒跨进程读文本轻量得多。变化时才回调，驱动前端刷新
//! 与同步引擎。
//!
//! 阶段四将替换为 NSPasteboard + NSPasteboardItemDataProvider 延迟渲染。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;

use super::types::{ClipboardContent, FileMeta, SyncId, WatchHandle};
use super::ClipboardProvider;

pub struct MacosClipboard {
    /// 进程级常驻 arboard 句柄（懒初始化）。复用同一实例避免反复创建开销，
    /// 也与 engine 复用常驻句柄的约定保持一致。
    inner: Arc<Mutex<Option<arboard::Clipboard>>>,
}

impl MacosClipboard {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait]
impl ClipboardProvider for MacosClipboard {
    async fn read(&self) -> Result<ClipboardContent> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.lock().unwrap();
            if guard.is_none() {
                *guard = Some(
                    arboard::Clipboard::new()
                        .map_err(|e| anyhow::anyhow!("无法访问 macOS 剪贴板: {e}"))?,
                );
            }
            let cb = guard.as_mut().unwrap();
            // 先试文本：成功即返回。
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
            let mut guard = inner.lock().unwrap();
            if guard.is_none() {
                *guard = Some(
                    arboard::Clipboard::new()
                        .map_err(|e| anyhow::anyhow!("无法访问 macOS 剪贴板: {e}"))?,
                );
            }
            let cb = guard.as_mut().unwrap();
            match content {
                ClipboardContent::Text(text) => cb.set_text(text)?,
                // 跨平台图片线格式未统一（见 linux.rs 注释），写入对端无法正确重建，先明确报错。
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

    async fn read_file_paths(&self) -> Result<Vec<PathBuf>> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.lock().unwrap();
            if guard.is_none() {
                *guard = Some(
                    arboard::Clipboard::new()
                        .map_err(|e| anyhow::anyhow!("无法访问 macOS 剪贴板: {e}"))?,
                );
            }
            let cb = guard.as_mut().unwrap();
            // arboard 读取 NSPasteboard 的 NSFilenamesPboardType（Finder 复制文件时写入）。
            // 无文件时返回 ContentNotAvailable，归一化为空向量，与 Windows 端 CF_HDROP 探测一致。
            // 关键修复：引擎先调本函数探测文件，命中即广播「可拉取」清单并跳过 read()，
            // 从而不再把 Finder 复制文件时附带的文件名文本误判为「粘贴板文字内容」。
            match cb.get().file_list() {
                Ok(paths) => Ok(paths),
                Err(arboard::Error::ContentNotAvailable) => Ok(Vec::new()),
                Err(e) => Err(anyhow::anyhow!("读取剪贴板文件列表失败: {e}")),
            }
        })
        .await
        .map_err(|e| anyhow::anyhow!("剪贴板文件读取任务失败: {e}"))?
    }

    async fn write_file_paths(&self, paths: &[PathBuf]) -> Result<()> {
        let inner = self.inner.clone();
        let paths = paths.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.lock().unwrap();
            if guard.is_none() {
                *guard = Some(
                    arboard::Clipboard::new()
                        .map_err(|e| anyhow::anyhow!("无法访问 macOS 剪贴板: {e}"))?,
                );
            }
            let cb = guard.as_mut().unwrap();
            // 把拉取完成、落盘到本地的文件写回剪贴板，使用户可在 Finder 中 Cmd+V 粘贴。
            // arboard 内部用 NSURL(fileURLWithPath:) + writeObjects 写入 NSFilenamesPboardType。
            cb.set()
                .file_list(&paths)
                .map_err(|e| anyhow::anyhow!("写入剪贴板文件列表失败: {e}"))
        })
        .await
        .map_err(|e| anyhow::anyhow!("剪贴板文件写入任务失败: {e}"))?
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
        anyhow::bail!("not yet implemented (phase 4)")
    }

    async fn watch(&self, cb: Box<dyn Fn() + Send>) -> Result<WatchHandle> {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        use objc2_app_kit::NSPasteboard;

        // 后台线程轮询 NSPasteboard.changeCount（仅比较自增整数，不读取剪贴板内容）。
        // 变化时才回调，驱动前端刷新与同步引擎。比在前端每秒跨进程读文本轻量得多。
        //
        // 注：Apple 官方建议 NSPasteboard 在主线程访问，但仅读取 changeCount 在后台线程
        // 实践中稳定可用；且引擎侧已用内容哈希去重兜底，误触发不会造成重复广播。
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        std::thread::spawn(move || {
            // 记录初始计数，避免进程启动时的首轮误触发。
            let mut last = unsafe { NSPasteboard::generalPasteboard().changeCount() };
            while !stop_thread.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                let count = unsafe { NSPasteboard::generalPasteboard().changeCount() };
                if count != last {
                    last = count;
                    cb();
                }
            }
        });
        // JoinHandle 在此丢弃 → 线程脱离（detached），仅由 stop 标志控制退出，
        // 与 Linux/Windows 端保持一致的 WatchHandle 语义（句柄存活期间持续监听）。
        Ok(WatchHandle::new(move || stop.store(true, Ordering::SeqCst)))
    }

    async fn has_sync_mark(&self, _sync_id: &SyncId) -> Result<bool> {
        Ok(false)
    }
}
