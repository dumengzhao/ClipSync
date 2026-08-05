//! 剪贴板相关类型定义

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// 强类型设备 ID
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

/// 强类型同步 ID
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SyncId(pub String);

/// 文件元数据（跨端统一传输结构）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub file_name: String,
    pub file_size: u64,
    pub is_dir: bool,
    pub relative_path: String,
    pub modified_at: u64,
    pub mime_type: String,
    pub hash: Option<String>,
}

/// 剪贴板内容类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClipboardContent {
    Text(String),
    Image {
        data: Vec<u8>,
        max_size: u32,
    },
    Files(Vec<FileMeta>),
    Html {
        html: String,
        text: String,
    },
}

/// 防回环同步标记
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncMark {
    pub device_id: DeviceId,
    pub sync_id: SyncId,
    pub timestamp: u64,
    pub lamport: u64,
    pub content_hash: String,
}

impl SyncMark {
    pub fn new(device_id: &DeviceId, content_hash: &str, lamport: u64) -> Self {
        Self {
            device_id: device_id.clone(),
            sync_id: SyncId(Uuid::new_v4().to_string()),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            lamport,
            content_hash: content_hash.to_string(),
        }
    }
}

/// 监听句柄
pub struct WatchHandle {
    _drop: Box<dyn Fn() + Send + Sync>,
}

impl WatchHandle {
    pub fn new(drop: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            _drop: Box::new(drop),
        }
    }
}
