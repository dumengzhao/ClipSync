//! 设备发现模块
//!
//! - `mdns`: 局域网自动发现
//! - `manual`: 手动地址连接

pub mod manual;
pub mod mdns;

use crate::clipboard::types::DeviceId;

/// 已发现的设备
#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub device_id: DeviceId,
    pub device_name: String,
    pub addr: String,
    pub port: u16,
    pub fingerprint: String,
}
