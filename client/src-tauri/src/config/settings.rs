//! 用户配置
//!
//! 注意：敏感字段（如密钥）不存于此，存于系统密钥链

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub device_name: String,
    pub auto_start: bool,
    pub sync_text: bool,
    pub sync_image: bool,
    pub sync_file: bool,
    pub max_file_size_mb: u64,
    pub max_image_size_mb: u32,
    pub listen_port: u16,
    pub enable_mdns: bool,
    pub manual_addresses: Vec<ManualAddress>,
    pub sync_primary_selection: bool,
    pub cache_ttl_hours: u32,
    pub theme: Theme,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualAddress {
    pub label: String,
    pub addr: String,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Theme {
    System,
    Light,
    Dark,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            device_name: "ClipSync-Device".to_string(),
            auto_start: true,
            sync_text: true,
            sync_image: true,
            sync_file: true,
            max_file_size_mb: 10 * 1024,
            max_image_size_mb: 50,
            listen_port: 24681,
            enable_mdns: true,
            manual_addresses: Vec::new(),
            sync_primary_selection: false,
            cache_ttl_hours: 24,
            theme: Theme::System,
        }
    }
}
