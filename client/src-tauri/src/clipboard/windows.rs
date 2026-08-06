//! Windows 剪贴板实现
//!
//! 使用 Win32 剪贴板 API（windows 0.58 位于 `Win32::System::DataExchange`）：
//! - 监听：MVP 阶段用轮询 `GetClipboardSequenceNumber` 检测变化
//!   （阶段三改为 AddClipboardFormatListener + 消息窗口事件）
//! - 读写：CF_UNICODETEXT（文本）/ CF_DIB（位图）
//! - 延迟渲染（CFSTR_FILEDESCRIPTOR + CFSTR_FILECONTENTS + IStream）为阶段三
//!
//! 注意：剪贴板数据句柄在 `DataExchange` 中以 `HANDLE` 表示，在 `Memory` 的
//! Global* 函数中以 `HGLOBAL` 表示，二者需互相转换。

use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, TRUE};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GHND};
use windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};
use windows::Win32::UI::Shell::DROPFILES;

/// 标准剪贴板格式：HDROP（文件/目录列表）。值为 15，直接用字面量避免类型歧义。
const CF_HDROP: u32 = 15;

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
        read_clipboard()
    }

    async fn write(&self, content: ClipboardContent) -> Result<()> {
        write_clipboard(content)
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
        anyhow::bail!("delayed file rendering not yet implemented (phase 3)")
    }

    async fn watch(&self, cb: Box<dyn Fn() + Send>) -> Result<WatchHandle> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        thread::spawn(move || {
            let mut last = current_clipboard_seq();
            while !stop_clone.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(300));
                let seq = current_clipboard_seq();
                if seq != last {
                    last = seq;
                    cb();
                }
            }
        });
        Ok(WatchHandle::new(move || stop.store(true, Ordering::SeqCst)))
    }

    async fn has_sync_mark(&self, _sync_id: &SyncId) -> Result<bool> {
        // 阶段三：通过自定义剪贴板格式嵌入同步标记后再实现防回环判定。
        Ok(false)
    }

    async fn read_file_paths(&self) -> Result<Vec<PathBuf>> {
        read_clipboard_files()
    }

    async fn write_file_paths(&self, paths: &[PathBuf]) -> Result<()> {
        write_clipboard_files(paths)
    }
}

/// 打开剪贴板，执行闭包，最后关闭剪贴板（无论成功失败）。
fn with_clipboard<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    unsafe {
        OpenClipboard(HWND::default())?;
    }
    let result = f();
    let _ = unsafe { CloseClipboard() };
    result
}

fn current_clipboard_seq() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

fn read_clipboard() -> Result<ClipboardContent> {
    with_clipboard(|| {
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32) }.is_ok() {
            let h = unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32)? };
            let hg = HGLOBAL(h.0);
            let ptr = unsafe { GlobalLock(hg) } as *const u16;
            if ptr.is_null() {
                return Err(anyhow!("GlobalLock failed"));
            }
            let len = unsafe {
                let mut len = 0usize;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                len
            };
            let text = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len)) };
            let _ = unsafe { GlobalUnlock(hg) };
            return Ok(ClipboardContent::Text(text));
        }

        if unsafe { IsClipboardFormatAvailable(CF_DIB.0 as u32) }.is_ok() {
            let h = unsafe { GetClipboardData(CF_DIB.0 as u32)? };
            let hg = HGLOBAL(h.0);
            let size = unsafe { GlobalSize(hg) };
            let ptr = unsafe { GlobalLock(hg) } as *const u8;
            if ptr.is_null() {
                return Err(anyhow!("GlobalLock failed"));
            }
            let data = unsafe { std::slice::from_raw_parts(ptr, size as usize).to_vec() };
            let _ = unsafe { GlobalUnlock(hg) };
            return Ok(ClipboardContent::Image {
                data,
                max_size: super::types::DEFAULT_MAX_IMAGE_SIZE,
            });
        }

        Err(anyhow!("clipboard contains no supported format"))
    })
}

fn write_clipboard(content: ClipboardContent) -> Result<()> {
    with_clipboard(|| {
        unsafe {
            EmptyClipboard()?;
        }
        match content {
            ClipboardContent::Text(text) => {
                let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                let h = unsafe { GlobalAlloc(GHND, wide.len() * 2)? };
                let ptr = unsafe { GlobalLock(h) } as *mut u16;
                if ptr.is_null() {
                    return Err(anyhow!("GlobalLock failed"));
                }
                unsafe { ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len()) };
                let _ = unsafe { GlobalUnlock(h) };
                let hnd = HANDLE(h.0);
                if unsafe { SetClipboardData(CF_UNICODETEXT.0 as u32, hnd)? }.is_invalid() {
                    return Err(anyhow!("SetClipboardData(CF_UNICODETEXT) failed"));
                }
                Ok(())
            }
            ClipboardContent::Image { data, .. } => {
                let h = unsafe { GlobalAlloc(GHND, data.len())? };
                let ptr = unsafe { GlobalLock(h) } as *mut u8;
                if ptr.is_null() {
                    return Err(anyhow!("GlobalLock failed"));
                }
                unsafe { ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len()) };
                let _ = unsafe { GlobalUnlock(h) };
                let hnd = HANDLE(h.0);
                if unsafe { SetClipboardData(CF_DIB.0 as u32, hnd)? }.is_invalid() {
                    return Err(anyhow!("SetClipboardData(CF_DIB) failed"));
                }
                Ok(())
            }
            other => Err(anyhow!(
                "unsupported clipboard content for write: {other:?}"
            )),
        }
    })
}

/// 读取剪贴板中的文件/目录列表（CF_HDROP）。非文件剪贴板返回空向量。
fn read_clipboard_files() -> Result<Vec<PathBuf>> {
    with_clipboard(|| {
        if !unsafe { IsClipboardFormatAvailable(CF_HDROP) }.is_ok() {
            return Ok(Vec::new());
        }
        let h = match unsafe { GetClipboardData(CF_HDROP) } {
            Ok(h) => h,
            Err(_) => return Ok(Vec::new()),
        };
        let hg = HGLOBAL(h.0);
        let ptr = unsafe { GlobalLock(hg) } as *const DROPFILES;
        if ptr.is_null() {
            return Ok(Vec::new());
        }
        let drop = unsafe { *ptr };
        // 文件列表位于 pFiles 偏移处，宽字符、双 \0 结尾
        let files_ptr = (ptr as *const u8).wrapping_add(drop.pFiles as usize) as *const u16;
        let mut paths = Vec::new();
        let mut cur = 0usize;
        loop {
            let mut end = cur;
            while unsafe { *files_ptr.add(end) } != 0 {
                end += 1;
            }
            if end == cur {
                break; // 空串 => 列表结束
            }
            let s = String::from_utf16_lossy(unsafe {
                std::slice::from_raw_parts(files_ptr.add(cur), end - cur)
            });
            paths.push(PathBuf::from(s));
            cur = end + 1;
        }
        let _ = unsafe { GlobalUnlock(hg) };
        Ok(paths)
    })
}

/// 把一组本地绝对路径写入剪贴板（CF_HDROP），使用户可直接 Ctrl+V 粘贴到资源管理器。
fn write_clipboard_files(paths: &[PathBuf]) -> Result<()> {
    with_clipboard(|| {
        unsafe {
            EmptyClipboard()?;
        }
        // DROPFILES 头 + 宽字符双 \0 结尾的文件列表
        let header_len = std::mem::size_of::<DROPFILES>();
        let mut encoded: Vec<u16> = Vec::new();
        for p in paths {
            let s = p.to_string_lossy();
            let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
            encoded.extend_from_slice(&wide);
        }
        encoded.push(0); // 列表结尾第二个 \0
        let total = header_len + encoded.len() * 2;
        let h = unsafe { GlobalAlloc(GHND, total)? };
        let ptr = unsafe { GlobalLock(h) } as *mut u8;
        if ptr.is_null() {
            return Err(anyhow!("GlobalLock failed"));
        }
        unsafe {
            let mut df = std::mem::zeroed::<DROPFILES>();
            df.pFiles = header_len as u32;
            df.fWide = TRUE;
            std::ptr::write(ptr as *mut DROPFILES, df);
            let list_ptr = ptr.add(header_len) as *mut u16;
            std::ptr::copy_nonoverlapping(encoded.as_ptr(), list_ptr, encoded.len());
            let _ = GlobalUnlock(h);
        }
        let hnd = HANDLE(h.0);
        if unsafe { SetClipboardData(CF_HDROP, hnd)? }.is_invalid() {
            return Err(anyhow!("SetClipboardData(CF_HDROP) failed"));
        }
        Ok(())
    })
}
