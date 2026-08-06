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
    /// 文件同步落盘目录（各端自选）。为空时回退到系统「下载」目录。
    /// 对端点击「拉取」后，文件下载到 `<sync_dir>/<对方设备名>/<相对路径>`。
    #[serde(default)]
    pub sync_dir: Option<String>,
    /// 自动拉取阈值（MB）。对端拷贝的文件/图片若总大小**小于**此值，本端收到后自动
    /// 拉取（下载到 `sync_dir` 并写本机剪贴板），无需手动点「拉取」。拷贝端自身除外。
    /// 默认 1MB；可调大让更大文件也自动拉取，但过大（如 >10MB）会占用较多带宽/磁盘。
    #[serde(default = "default_auto_pull_threshold_mb")]
    pub auto_pull_threshold_mb: u64,
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

fn default_auto_pull_threshold_mb() -> u64 {
    1
}

/// 默认设备名取本机机器名（hostname），使不同设备默认即可区分。
/// 取不到（或为空）时回退到固定占位名，避免空名称。
pub fn default_device_name() -> String {
    let host = gethostname::gethostname().to_string_lossy().trim().to_string();
    if host.is_empty() {
        "ClipSync-Device".to_string()
    } else {
        host
    }
}

/// 旧版默认设备名（写死的占位串）。用于识别并迁移历史配置。
pub const LEGACY_DEFAULT_DEVICE_NAME: &str = "ClipSync-Device";

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            device_name: default_device_name(),
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
            sync_dir: None,
            auto_pull_threshold_mb: 1,
            theme: Theme::System,
        }
    }
}

impl AppConfig {
    /// 自动拉取阈值（字节）。`auto_pull_threshold_mb` 为 0 时退化为「不自动拉取」。
    pub fn auto_pull_threshold_bytes(&self) -> u64 {
        self.auto_pull_threshold_mb.saturating_mul(1024 * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_pull_threshold_default_is_1mb() {
        let c = AppConfig::default();
        assert_eq!(c.auto_pull_threshold_mb, 1);
        assert_eq!(c.auto_pull_threshold_bytes(), 1024 * 1024);
    }

    #[test]
    fn auto_pull_threshold_bytes_scales() {
        let c = AppConfig {
            auto_pull_threshold_mb: 10,
            ..AppConfig::default()
        };
        assert_eq!(c.auto_pull_threshold_bytes(), 10 * 1024 * 1024);
        let z = AppConfig {
            auto_pull_threshold_mb: 0,
            ..AppConfig::default()
        };
        assert_eq!(z.auto_pull_threshold_bytes(), 0);
    }
}
