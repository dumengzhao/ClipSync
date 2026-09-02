//! 用户配置
//!
//! 注意：敏感字段（如密钥）不存于此，存于系统密钥链

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub device_name: String,
    pub auto_start: bool,
    /// 开机自启后是否显示主窗口（仅当 `auto_start` 为真时生效）。默认显示。
    #[serde(default = "default_true")]
    pub show_main_window_on_launch: bool,
    pub sync_text: bool,
    pub sync_image: bool,
    pub sync_file: bool,
    pub max_file_size_mb: u64,
    pub max_image_size_mb: u32,
    pub listen_port: u16,
    pub enable_mdns: bool,
    /// SPAKE2 配对口令（即设置中的「预留配对码」）。**两端必须设置成完全相同的值**，
    /// 才能互相配对；它同时作为首配对与重连的口令，不再每次随机生成。
    /// 默认值仅用于开发/测试；正式使用请在各端手动设置同一串强口令。
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
    /// 是否开启「自动拉取」。关闭后，对端传来的任何文件都**不会**自动拉取，
    /// 必须手动点「拉取」；此开关优先于阈值——即便阈值很大，关闭时也一律不自动拉取。
    /// 默认关闭（需用户在设置页主动开启）。
    #[serde(default = "default_false")]
    pub auto_pull_enabled: bool,
    /// 自动拉取阈值（MB）。仅当 `auto_pull_enabled` 开启、且对端拷贝的文件/图片总大小
    /// **小于**此值时，本端收到后才自动拉取（下载到 `sync_dir` 并写本机剪贴板），
    /// 无需手动点「拉取」。拷贝端自身除外。
    /// 默认 1MB；可调大让更大文件也自动拉取，但过大（如 >10MB）会占用较多带宽/磁盘。
    #[serde(default = "default_auto_pull_threshold_mb")]
    pub auto_pull_threshold_mb: u64,
    /// 本机复制文件夹时，递归文件数超过此上限则**不推送**给对端，仅本地提示「请压缩后复制」。
    /// 0 视为「不限制」（任何大小的文件夹都正常同步）。默认 100。
    #[serde(default = "default_max_folder_files")]
    pub max_folder_files: usize,
    // ===== 跨局域网中转（服务端）相关配置 =====
    /// 服务端 WebSocket 地址，例如 `ws://your-host:20070/ws`。为空表示不使用服务端（仅局域网直连）。
    #[serde(default)]
    pub server_url: String,
    /// Network Token（共享密钥），用于服务端鉴权 + 跨 LAN 文字端到端加密。
    #[serde(default)]
    pub network_token: String,
    /// 本机在对端眼中用于文件直取的对外地址 `ip:port`（公网可达）。为空则跨 LAN 文件不可拉取。
    #[serde(default)]
    pub ext_file_ep: String,
    /// 局域网分组标识：相同值视为同一局域网（走直连），不同值视为跨局域网（走服务端）。
    /// 为空时按本机网段自动推断。
    #[serde(default)]
    pub lan_group: String,
    /// 待拉取小窗「未操作自动关闭」时长（毫秒）。
    ///
    /// 规则：小窗弹出后，若用户在这段时间内**没有点击「拉取」**，则自动收起；
    /// 一旦点击了拉取，就取消该倒计时，改为**等拉取完成并写入本机剪贴板之后**才关闭
    /// （拉取中绝不关闭）。默认 15000（15 秒）。设为 0 表示「不自动关闭」。
    #[serde(default = "default_toast_auto_hide_ms")]
    pub toast_auto_hide_ms: u64,
    /// 主窗口默认宽高（逻辑像素）。为空（None）时采用 tauri.conf.json 的默认尺寸（即「当前宽高」）；
    /// 仅当 `window_width` 与 `window_height` 同时设置（均非 None）时才生效，任一缺失则沿用默认尺寸。
    #[serde(default)]
    pub window_width: Option<u32>,
    /// 主窗口默认高度（逻辑像素）。含义同 `window_width`。
    #[serde(default)]
    pub window_height: Option<u32>,
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

fn default_max_folder_files() -> usize {
    100
}

/// 待拉取小窗未操作自动关闭时长（毫秒），默认 15 秒。
fn default_toast_auto_hide_ms() -> u64 {
    15_000
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
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
            auto_start: false,
            sync_text: true,
            sync_image: true,
            sync_file: true,
            max_file_size_mb: 10 * 1024,
            max_image_size_mb: 50,
            listen_port: 20071,
            enable_mdns: true,
            pairing_code: "000000".to_string(),
            manual_addresses: Vec::new(),
            sync_primary_selection: false,
            cache_ttl_hours: 24,
            sync_dir: None,
            auto_pull_enabled: false,
            auto_pull_threshold_mb: 1,
            max_folder_files: 100,
            theme: Theme::System,
            server_url: String::new(),
            network_token: String::new(),
            ext_file_ep: String::new(),
            lan_group: String::new(),
            show_main_window_on_launch: true,
            toast_auto_hide_ms: default_toast_auto_hide_ms(),
            window_width: None,
            window_height: None,
        }
    }
}

impl AppConfig {
    /// 自动拉取阈值（字节）。
    pub fn auto_pull_threshold_bytes(&self) -> u64 {
        self.auto_pull_threshold_mb.saturating_mul(1024 * 1024)
    }

    /// 是否应自动拉取：总开关 `auto_pull_enabled` 开启、且 `total_bytes` 严格小于阈值。
    /// 拷贝端自身由调用方另行排除。阈值设为 0 时 `total < 0` 永不成立，自然不自动拉取。
    pub fn should_auto_pull(&self, total_bytes: u64) -> bool {
        self.auto_pull_enabled && total_bytes < self.auto_pull_threshold_bytes()
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
