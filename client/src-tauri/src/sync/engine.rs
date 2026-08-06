//! 同步引擎 - 协调各模块
//!
//! 阶段一实现：剪贴板监听 → 防回环标记 → 事件广播。
//! 引擎持有 AntiLoop、设备身份与平台剪贴板，将本地剪贴板变化以 `SyncEvent` 形式
//! 通过 tokio broadcast 广播，并由消费任务转译为 Tauri 事件 `clipboard-changed`。

use crate::clipboard::types::{DeviceId, SyncId, SyncMark};
use crate::clipboard::{ClipboardContent, ClipboardProvider, PlatformClipboard};
use crate::device::identity::DeviceIdentity;
use crate::sync::anti_loop::{content_hash, AntiLoop};
use anyhow::Result;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};

/// 引擎对外广播的剪贴板变化事件
#[derive(Debug, Clone)]
pub enum SyncEvent {
    LocalClipboardChanged {
        /// 本次写出的同步标记（含 sync_id / device_id / lamport / 内容哈希）
        mark: SyncMark,
        content: ClipboardContent,
    },
}

/// 转发给前端的轻量事件负载（避免大图片字节进入事件通道）
#[derive(Debug, Clone, Serialize)]
pub struct ClipboardChangedPayload {
    pub sync_id: String,
    pub device_id: String,
    pub kind: String,
    pub text: Option<String>,
    pub size: Option<u64>,
}

impl ClipboardChangedPayload {
    pub fn from_content(
        sync_id: &SyncId,
        device_id: &DeviceId,
        content: &ClipboardContent,
    ) -> Self {
        let (kind, text, size) = match content {
            ClipboardContent::Text(t) => ("text", Some(t.clone()), None),
            ClipboardContent::Image { data, .. } => ("image", None, Some(data.len() as u64)),
            ClipboardContent::Files(files) => {
                ("files", None, Some(files.iter().map(|f| f.file_size).sum()))
            }
            ClipboardContent::Html { text, .. } => ("html", Some(text.clone()), None),
        };
        Self {
            sync_id: sync_id.0.clone(),
            device_id: device_id.0.clone(),
            kind: kind.to_string(),
            text,
            size,
        }
    }
}

pub struct SyncEngine {
    anti_loop: Arc<AntiLoop>,
    identity: Arc<DeviceIdentity>,
    signal_tx: tokio::sync::broadcast::Sender<()>,
    event_tx: tokio::sync::broadcast::Sender<SyncEvent>,
    last_emitted: Arc<Mutex<Option<String>>>,
}

impl SyncEngine {
    pub fn new(identity: DeviceIdentity) -> Self {
        let (signal_tx, _) = tokio::sync::broadcast::channel::<()>(16);
        let (event_tx, _) = tokio::sync::broadcast::channel::<SyncEvent>(64);
        Self {
            anti_loop: Arc::new(AntiLoop::new()),
            identity: Arc::new(identity),
            signal_tx,
            event_tx,
            last_emitted: Arc::new(Mutex::new(None)),
        }
    }

    /// 订阅引擎广播事件（用于测试或扩展消费者）
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<SyncEvent> {
        self.event_tx.subscribe()
    }

    /// 启动引擎：开始监听剪贴板并将变化广播为 Tauri 事件
    pub async fn start(&self, app: AppHandle) -> Result<()> {
        // 1) 启动剪贴板监听；变化时通过 signal 通道（非阻塞）通知处理任务
        let signal_tx = self.signal_tx.clone();
        let watch = PlatformClipboard::new()
            .watch(Box::new(move || {
                let _ = signal_tx.send(());
            }))
            .await?;
        // watch 句柄保持存活到进程结束（剪贴板监听线程持续运行）
        std::mem::forget(watch);

        // 2) 处理任务：读取剪贴板 → 防回环标记 → 广播
        let mut signal_rx = self.signal_tx.subscribe();
        let anti_loop = self.anti_loop.clone();
        let identity = self.identity.clone();
        let event_tx = self.event_tx.clone();
        let last_emitted = self.last_emitted.clone();
        tauri::async_runtime::spawn(async move {
            while signal_rx.recv().await.is_ok() {
                let clipboard = PlatformClipboard::new();
                let Ok(content) = clipboard.read().await else {
                    continue;
                };
                let hash = content_hash(&content);
                let should_emit = {
                    let mut g = last_emitted.lock().unwrap();
                    if g.as_ref() == Some(&hash) {
                        false
                    } else {
                        *g = Some(hash);
                        true
                    }
                };
                if !should_emit {
                    continue;
                }
                let device_id = identity.id.clone();
                let mark = anti_loop.mark_outgoing(&device_id, &content);
                let _ = event_tx.send(SyncEvent::LocalClipboardChanged { mark, content });
            }
        });

        // 3) 消费任务：将广播事件转译为 Tauri 前端事件
        let mut event_rx = self.event_tx.subscribe();
        let app_handle = app.clone();
        tauri::async_runtime::spawn(async move {
            while let Ok(ev) = event_rx.recv().await {
                let SyncEvent::LocalClipboardChanged { mark, content } = ev;
                let payload =
                    ClipboardChangedPayload::from_content(&mark.sync_id, &mark.device_id, &content);
                let _ = app_handle.emit("clipboard-changed", payload);
            }
        });

        Ok(())
    }

    /// 应用对端传来的剪贴板更新：先记录防回环标记（推进 Lamport 时钟），
    /// 再写回本地剪贴板。写入前把内容哈希登记为「最近已发出」，使本机剪贴板监听
    /// 回调触发的回声被判定为重复而丢弃，避免回环。
    pub async fn apply_remote(&self, mark: SyncMark, content: ClipboardContent) {
        self.anti_loop.record_applied(&mark);
        {
            let mut g = self.last_emitted.lock().unwrap();
            *g = Some(content_hash(&content));
        }
        if let Err(e) = PlatformClipboard::new().write(content).await {
            tracing::error!("failed to write remote clipboard content: {e}");
        }
    }
}
