//! 设备发现模块
//!
//! - `mdns`: 局域网自动发现（DNS-SD，端口从对端广告动态学习，不写死）
//! - `manual`: 手动地址连接（跨网或 mDNS 不可用时由用户手填 IP:端口）

pub mod manual;
pub mod mdns;

pub use mdns::MdnsDiscovery;

use crate::clipboard::types::DeviceId;
use serde::Serialize;

/// 已发现的设备（手动地址簿使用，含 fingerprint 占位）
#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub device_id: DeviceId,
    pub device_name: String,
    pub addr: String,
    pub port: u16,
    pub fingerprint: String,
}

/// 通过 mDNS 发现的局域网对端，前端可直接消费（序列化后作为事件负载）
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredPeer {
    pub device_id: String,
    pub device_name: String,
    pub addr: String,
    /// 端口来自对端 mDNS 广告的 SRV 记录，由发现方动态读取
    pub port: u16,
}
