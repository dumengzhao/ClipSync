//! 手动地址连接 - 地址簿管理
//!
//! 跨网或 mDNS 不可用时，由用户手动添加对端地址（IP:Port 或域名:Port）直连。
//! MVP 阶段为内存存储，按标签（label）唯一索引。

use crate::config::settings::ManualAddress;
use std::collections::HashMap;

pub struct ManualAddressBook {
    addresses: HashMap<String, ManualAddress>,
}

impl ManualAddressBook {
    pub fn new() -> Self {
        Self {
            addresses: HashMap::new(),
        }
    }

    pub fn add(&mut self, addr: ManualAddress) {
        self.addresses.insert(addr.label.clone(), addr);
    }

    pub fn remove(&mut self, label: &str) -> bool {
        self.addresses.remove(label).is_some()
    }

    pub fn list(&self) -> Vec<ManualAddress> {
        self.addresses.values().cloned().collect()
    }

    pub fn get(&self, label: &str) -> Option<&ManualAddress> {
        self.addresses.get(label)
    }

    pub fn len(&self) -> usize {
        self.addresses.len()
    }

    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty()
    }
}

impl Default for ManualAddressBook {
    fn default() -> Self {
        Self::new()
    }
}
