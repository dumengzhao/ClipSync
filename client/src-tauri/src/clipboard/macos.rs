//! macOS 剪贴板实现
//!
//! 阶段四实现：
//! - NSPasteboard generalPasteboard
//! - NSPasteboardDidChangeNotification / changeCount 轮询
//! - NSPasteboardItemDataProvider 延迟渲染

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use super::types::{ClipboardContent, FileMeta, SyncId, WatchHandle};
use super::ClipboardProvider;

pub struct MacosClipboard;

impl MacosClipboard {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ClipboardProvider for MacosClipboard {
    async fn read(&self) -> Result<ClipboardContent> {
        anyhow::bail!("not yet implemented (phase 1)")
    }

    async fn write(&self, _content: ClipboardContent) -> Result<()> {
        anyhow::bail!("not yet implemented (phase 1)")
    }

    async fn read_file_paths(&self) -> Result<Vec<PathBuf>> {
        anyhow::bail!("not yet implemented (phase 2)")
    }

    async fn write_file_paths(&self, _paths: &[PathBuf]) -> Result<()> {
        anyhow::bail!("not yet implemented (phase 2)")
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

    async fn watch(&self, _cb: Box<dyn Fn() + Send>) -> Result<WatchHandle> {
        anyhow::bail!("not yet implemented (phase 1)")
    }

    async fn has_sync_mark(&self, _sync_id: &SyncId) -> Result<bool> {
        Ok(false)
    }
}
