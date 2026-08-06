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
    /// **进程级唯一**的剪贴板句柄，全生命周期保持存活。
    ///
    /// 绝不能每次读写都 `PlatformClipboard::new()` 后立即丢弃：Windows 下剪贴板数据由
    /// 系统持有，临时实例无妨；但 **X11 / Wayland 的剪贴板是「所有权」模型** —— 写入方
    /// 必须作为 selection owner 常驻，持续应答其它进程的 SelectionRequest。实例一旦被
    /// Drop，所有权即刻释放，对端粘贴只会拿到空内容。共用一个长生命周期实例是跨平台
    /// 正确写入的前提。
    clipboard: Arc<PlatformClipboard>,
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
            clipboard: Arc::new(PlatformClipboard::new()),
            signal_tx,
            event_tx,
            last_emitted: Arc::new(Mutex::new(None)),
        }
    }

    /// 订阅引擎广播事件（用于测试或扩展消费者）
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<SyncEvent> {
        self.event_tx.subscribe()
    }

    /// 取得进程内唯一的剪贴板句柄。
    ///
    /// 所有剪贴板读写都必须走它，不要另建临时实例——原因见 `clipboard` 字段注释。
    pub fn clipboard(&self) -> Arc<PlatformClipboard> {
        self.clipboard.clone()
    }

    /// 启动引擎：开始监听剪贴板并将变化广播为 Tauri 事件
    pub async fn start(&self, app: AppHandle) -> Result<()> {
        // 1) 启动剪贴板监听；变化时通过 signal 通道（非阻塞）通知处理任务
        let signal_tx = self.signal_tx.clone();
        let watch = self
            .clipboard
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
        let clipboard = self.clipboard.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                // 注意：不能写成 `while recv().await.is_ok()`。broadcast 在接收方落后时
                // 返回 `Lagged`，那样会让循环**永久退出**，本机从此再也无法把剪贴板发出去
                // （而接收远端内容走的是另一条代码路径，仍然正常）——表现为「单向能同步」。
                match signal_rx.recv().await {
                    Ok(()) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("剪贴板信号积压，跳过 {n} 次变化");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
                let content = match clipboard.read().await {
                    Ok(c) => c,
                    Err(e) => {
                        // 复制了不支持的格式（如 Windows 上的文件 CF_HDROP），或读取被
                        // 其它进程抢占。必须留痕，否则整条链路静默丢内容、无从排查。
                        tracing::warn!("读取本地剪贴板失败，本次变化已忽略: {e}");
                        continue;
                    }
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
                    tracing::debug!("剪贴板内容与上次相同（或为远端回声），跳过");
                    continue;
                }
                let device_id = identity.id.clone();
                let mark = anti_loop.mark_outgoing(&device_id, &content);
                tracing::debug!(
                    "本地剪贴板变化 kind={} sync_id={}",
                    content_kind(&content),
                    mark.sync_id.0
                );
                let _ = event_tx.send(SyncEvent::LocalClipboardChanged { mark, content });
            }
        });

        // 3) 消费任务：将广播事件转译为 Tauri 前端事件
        let mut event_rx = self.event_tx.subscribe();
        let app_handle = app.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                let ev = match event_rx.recv().await {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
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
        let kind = content_kind(&content);
        // 用常驻实例写入：X11/Wayland 下临时实例被 Drop 会立即失去 selection 所有权，
        // 对端粘贴将拿到空内容。
        match self.clipboard.write(content).await {
            Ok(()) => tracing::debug!("已应用来自 {} 的剪贴板内容 kind={kind}", mark.device_id.0),
            Err(e) => tracing::error!("写入远端剪贴板内容失败 kind={kind}: {e}"),
        }
    }
}

/// 内容类型的简短名称，仅用于日志
fn content_kind(content: &ClipboardContent) -> &'static str {
    match content {
        ClipboardContent::Text(_) => "text",
        ClipboardContent::Image { .. } => "image",
        ClipboardContent::Files(_) => "files",
        ClipboardContent::Html { .. } => "html",
    }
}
