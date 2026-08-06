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
    /// SPAKE2 配对码（6 位数字）。两端设成相同即可自动配对。
    /// 默认值仅用于开发/测试；正式使用应在各端设置不同码并通过 UI 完成配对。
    #[serde(default = "default_pairing_code")]
    pub pairing_code: String,
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

fn default_pairing_code() -> String {
    "000000".to_string()
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
            pairing_code: "000000".to_string(),
            manual_addresses: Vec::new(),
            sync_primary_selection: false,
            cache_ttl_hours: 24,
            theme: Theme::System,
        }
    }
}
