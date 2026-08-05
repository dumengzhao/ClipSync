//! Windows 剪贴板实现
//!
//! 阶段三实现：
//! - AddClipboardFormatListener 监听
//! - CF_UNICODETEXT / CF_DIB / CF_HDROP 读写
//! - CFSTR_FILEDESCRIPTOR + CFSTR_FILECONTENTS 延迟渲染
//! - IStream + IDataObject COM 接口

use anyhow::Result;
use async_trait::async_trait;

use super::types::{ClipboardContent, FileMeta, SyncId, WatchHandle};
use super::ClipboardProvider;

pub struct WindowsClipboard;

impl WindowsClipboard {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ClipboardProvider for WindowsClipboard {
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
        anyhow::bail!("not yet implemented (phase 3)")
    }

    async fn watch(&self, _cb: Box<dyn Fn() + Send>) -> Result<WatchHandle> {
        anyhow::bail!("not yet implemented (phase 1)")
    }

    async fn has_sync_mark(&self, _sync_id: &SyncId) -> Result<bool> {
        Ok(false)
    }
}
