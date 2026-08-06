//! 已配对设备注册表 - 内存存储
//!
//! MVP 阶段为内存存储；持久化（密钥链 + 配置文件）在后续阶段接入。
//! 通过唯一 `DeviceId` 索引，支持增删查与列表枚举。

use crate::clipboard::types::DeviceId;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLevel {
    Unverified,
    Verified,
}

#[derive(Debug, Clone)]
pub struct PairedDevice {
    pub device_id: DeviceId,
    pub device_name: String,
    /// 对端公钥指纹（SHA-256 前 16 字节十六进制），供用户人工核对
    pub fingerprint: String,
    pub trust: TrustLevel,
    /// 最后在线时间戳（unix seconds）
    pub last_seen: u64,
}

pub struct DeviceRegistry {
    devices: HashMap<DeviceId, PairedDevice>,
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self {
            devices: HashMap::new(),
        }
    }

    pub fn add(&mut self, device: PairedDevice) {
        self.devices.insert(device.device_id.clone(), device);
    }

    pub fn remove(&mut self, id: &DeviceId) -> bool {
        self.devices.remove(id).is_some()
    }

    pub fn get(&self, id: &DeviceId) -> Option<&PairedDevice> {
        self.devices.get(id)
    }

    pub fn list(&self) -> Vec<PairedDevice> {
        self.devices.values().cloned().collect()
    }

    pub fn contains(&self, id: &DeviceId) -> bool {
        self.devices.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}
