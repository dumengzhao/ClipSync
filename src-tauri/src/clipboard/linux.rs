//! Linux 剪贴板实现（X11 + Wayland）
//!
//! 阶段五实现：
//! - X11: x11rb SelectionRequest 处理
//! - Wayland: smithay-client-toolkit wl_data_source
//! - primary selection 支持（可选）

use anyhow::Result;
use async_trait::async_trait;

use super::types::{ClipboardContent, FileMeta, SyncId, WatchHandle};
use super::ClipboardProvider;

pub struct LinuxClipboard;

impl LinuxClipboard {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ClipboardProvider for LinuxClipboard {
    async fn read(&self) -> Result<ClipboardContent> {
        anyhow::bail!("not yet implemented (phase 1)")
    }

    async fn write(&self, _content: ClipboardContent) -> Result<()> {
        anyhow::bail!("not yet implemented (phase 1)")
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
        anyhow::bail!("not yet implemented (phase 5)")
    }

    async fn watch(&self, _cb: Box<dyn Fn() + Send>) -> Result<WatchHandle> {
        anyhow::bail!("not yet implemented (phase 1)")
    }

    async fn has_sync_mark(&self, _sync_id: &SyncId) -> Result<bool> {
        Ok(false)
    }
}
