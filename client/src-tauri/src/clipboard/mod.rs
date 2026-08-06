//! 剪贴板模块 - 跨平台抽象
//!
//! 各平台实现见子模块：
//! - `windows.rs`: Win32 API + COM IStream
//! - `macos.rs`: NSPasteboard + DataProvider
//! - `linux.rs`: X11 + Wayland

pub mod types;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

use anyhow::Result;
use async_trait::async_trait;

pub use types::{ClipboardContent, DeviceId, FileMeta, SyncId, SyncMark, WatchHandle};

/// 跨平台剪贴板统一接口
#[async_trait]
pub trait ClipboardProvider: Send + Sync {
    /// 读取当前剪贴板内容
    async fn read(&self) -> Result<ClipboardContent>;

    /// 写入剪贴板（基础模式）
    async fn write(&self, content: ClipboardContent) -> Result<()>;

    /// 延迟渲染模式写入文件
    async fn write_delayed_files<F>(
        &self,
        files: Vec<FileMeta>,
        sync_id: SyncId,
        fetch_cb: F,
    ) -> Result<()>
    where
        F: Fn(usize, u64, u32) -> Result<Vec<u8>> + Send + Sync + 'static;

    /// 监听剪贴板变化
    async fn watch(&self, cb: Box<dyn Fn() + Send>) -> Result<WatchHandle>;

    /// 检查剪贴板是否包含同步标记（防回环）
    async fn has_sync_mark(&self, sync_id: &SyncId) -> Result<bool>;
}

/// 平台特定的剪贴板实现
#[cfg(target_os = "windows")]
pub type PlatformClipboard = windows::WindowsClipboard;
#[cfg(target_os = "macos")]
pub type PlatformClipboard = macos::MacosClipboard;
#[cfg(target_os = "linux")]
pub type PlatformClipboard = linux::LinuxClipboard;
