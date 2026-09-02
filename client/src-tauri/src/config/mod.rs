//! 配置模块

pub mod migration;
pub mod settings;

pub use settings::AppConfig;

use std::path::PathBuf;
use tauri::Manager;

/// 配置文件名
const CONFIG_FILE: &str = "config.json";
/// 配置所在子目录名（位于用户主目录下，跨平台统一；由 home_dir() 解析，不写死平台路径）
const CONFIG_DIR_NAME: &str = "ClipSync";

/// 计算 ClipSync 配置根目录：`<home_dir()>/ClipSync`。
/// 使用 Tauri 的 `home_dir()` 平台专属函数获取用户主目录，各平台自动对应：
/// macOS -> /Users/<user>/ClipSync，Linux -> /home/<user>/ClipSync，Windows -> C:\Users\<user>\ClipSync。
/// 该目录位于用户主目录、非应用包内，重装/升级均不影响配置。
/// 其它模块（设备配对存储等）也复用此函数，保证所有持久化文件统一落在同一位置。
pub fn clipsync_base_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path()
        .home_dir()
        .ok()
        .map(|home| home.join(CONFIG_DIR_NAME))
}

/// 计算配置文件路径：`<clipsync_base_dir()>/config.json>`。
fn config_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    clipsync_base_dir(app).map(|dir| dir.join(CONFIG_FILE))
}

/// 从旧位置（应用配置目录 `<app_config_dir>/config.json`）迁移已有配置到新的用户主目录位置（仅一次）。
/// 保证升级/重装后历史配置（服务端地址、令牌、手动地址、窗口默认尺寸等）不丢失；
/// 新位置已存在文件或旧位置无文件则跳过。
fn migrate_from_legacy(app: &tauri::AppHandle, new_path: &std::path::Path) {
    if new_path.exists() {
        return;
    }
    if let Some(old) = app
        .path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join(CONFIG_FILE))
    {
        if old.exists() {
            if let Some(parent) = new_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::copy(&old, new_path) {
                Ok(_) => tracing::info!("已从旧位置迁移配置到 {}", new_path.display()),
                Err(e) => tracing::warn!("迁移旧配置失败（不影响启动）: {e}"),
            }
        }
    }
}

/// 从磁盘加载配置；文件不存在或解析失败时回退到默认配置（不报错）。
pub fn load_config(app: &tauri::AppHandle) -> AppConfig {
    let Some(path) = config_path(app) else {
        return AppConfig::default();
    };
    // 升级迁移：将旧路径（应用配置目录）下的历史配置一次性搬到新的用户主目录位置。
    migrate_from_legacy(app, &path);
    let cfg = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("config parse failed ({}), falling back to defaults", e);
            AppConfig::default()
        }),
        Err(_) => AppConfig::default(),
    };
    migrate_device_name(app, cfg)
}

/// 迁移历史/无效的设备名：空名或旧版固定占位名（`ClipSync-Device`）重新生成为
/// 本机机器名，使不同设备默认即可区分；并把结果落盘，避免每次启动重复迁移。
fn migrate_device_name(app: &tauri::AppHandle, mut cfg: AppConfig) -> AppConfig {
    let needs_migrate = cfg.device_name.trim().is_empty()
        || cfg.device_name == settings::LEGACY_DEFAULT_DEVICE_NAME;
    if needs_migrate {
        cfg.device_name = settings::default_device_name();
        // 静默落盘；失败不影响本次启动（下次启动仍会重新生成）
        let _ = save_config(app, &cfg);
    }
    cfg
}

/// 将配置写入磁盘（用户主目录下的 ClipSync/ 目录），供重启后依然生效。
/// 该目录位于用户主目录、非应用包内，重装/升级均不影响配置。
pub fn save_config(app: &tauri::AppHandle, cfg: &AppConfig) -> anyhow::Result<()> {
    let path = config_path(app)
        .ok_or_else(|| anyhow::anyhow!("无法确定用户主目录，无法写入配置"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}
