//! 防回环模块
//!
//! 通过 Lamport 时钟 + 内容哈希，判定一条剪贴板更新是否已经由本机处理过，
//! 避免同步环路（A → B → A）。纯逻辑，可单元测试。

use std::sync::Mutex;

use crate::clipboard::types::{ClipboardContent, DeviceId, SyncId, SyncMark};

/// 计算剪贴板内容的稳定哈希（用于防回环与冲突判定）
pub fn content_hash(content: &ClipboardContent) -> String {
    use blake3::Hasher;
    let mut hasher = Hasher::new();
    match content {
        ClipboardContent::Text(t) => {
            hasher.update(t.as_bytes());
        }
        ClipboardContent::Image { data, .. } => {
            hasher.update(data);
        }
        ClipboardContent::Files(files) => {
            for f in files {
                hasher.update(f.file_name.as_bytes());
                hasher.update(f.relative_path.as_bytes());
            }
        }
        ClipboardContent::Html { html, text } => {
            hasher.update(html.as_bytes());
            hasher.update(text.as_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// 防回环状态机
pub struct AntiLoop {
    inner: Mutex<Inner>,
}

struct Inner {
    lamport: u64,
    last_outgoing_sync_id: Option<SyncId>,
    last_content_hash: Option<String>,
}

impl AntiLoop {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                lamport: 0,
                last_outgoing_sync_id: None,
                last_content_hash: None,
            }),
        }
    }

    /// 生成本机下一次写出的同步标记，并推进 Lamport 时钟
    pub fn mark_outgoing(&self, device_id: &DeviceId, content: &ClipboardContent) -> SyncMark {
        let mut g = self.inner.lock().unwrap();
        g.lamport += 1;
        let hash = content_hash(content);
        g.last_content_hash = Some(hash.clone());
        let mark = SyncMark::new(device_id, &hash, g.lamport);
        // 记录本次写出的 sync_id，供 is_loopback 判定自己的回环
        g.last_outgoing_sync_id = Some(mark.sync_id.clone());
        mark
    }

    /// 判定传入的更新是否为本机刚写出的回环（sync_id 匹配）
    pub fn is_loopback(&self, sync_id: &SyncId) -> bool {
        let g = self.inner.lock().unwrap();
        g.last_outgoing_sync_id.as_ref() == Some(sync_id)
    }

    /// 记录已应用的远端同步标记（更新 Lamport 上界并记录来源 sync_id）
    pub fn record_applied(&self, mark: &SyncMark) {
        let mut g = self.inner.lock().unwrap();
        if mark.lamport > g.lamport {
            g.lamport = mark.lamport;
        }
        g.last_outgoing_sync_id = Some(mark.sync_id.clone());
        g.last_content_hash = Some(mark.content_hash.clone());
    }
}

impl Default for AntiLoop {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_stable() {
        let a = ClipboardContent::Text("hello".into());
        let b = ClipboardContent::Text("hello".into());
        let c = ClipboardContent::Text("world".into());
        assert_eq!(content_hash(&a), content_hash(&b));
        assert_ne!(content_hash(&a), content_hash(&c));
    }

    #[test]
    fn lamport_increments_on_outgoing() {
        let al = AntiLoop::new();
        let dev = DeviceId("dev-a".into());
        let m1 = al.mark_outgoing(&dev, &ClipboardContent::Text("x".into()));
        // 刚写出的标记应被判定为回环（自己的回声）
        assert!(al.is_loopback(&m1.sync_id));
        let m2 = al.mark_outgoing(&dev, &ClipboardContent::Text("y".into()));
        assert_eq!(m2.lamport, m1.lamport + 1);
        // m2 覆盖 m1，成为最新的 outgoing
        assert!(al.is_loopback(&m2.sync_id));
        assert!(!al.is_loopback(&m1.sync_id));
    }

    #[test]
    fn record_applied_advances_lamport() {
        let al = AntiLoop::new();
        let mark = SyncMark {
            device_id: DeviceId("dev-b".into()),
            sync_id: SyncId("remote-1".into()),
            timestamp: 0,
            lamport: 42,
            content_hash: "abc".into(),
        };
        al.record_applied(&mark);
        let next = al.mark_outgoing(
            &DeviceId("dev-a".into()),
            &ClipboardContent::Text("z".into()),
        );
        assert_eq!(next.lamport, 43);
        assert!(!al.is_loopback(&SyncId("remote-1".into())));
    }
}
