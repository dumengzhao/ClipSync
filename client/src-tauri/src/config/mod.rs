//! 配置模块

pub mod migration;
pub mod settings;

pub use settings::AppConfig;

use std::path::PathBuf;
use tauri::Manager;

/// 配置文件名（位于应用配置目录下）
const CONFIG_FILE: &str = "config.json";

/// 计算配置文件路径：`<app_config_dir>/config.json>`
fn config_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join(CONFIG_FILE))
}

/// 从磁盘加载配置；文件不存在或解析失败时回退到默认配置（不报错）。
pub fn load_config(app: &tauri::AppHandle) -> AppConfig {
    let Some(path) = config_path(app) else {
        return AppConfig::default();
    };
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

/// 将配置写入磁盘（应用配置目录下），供重启后依然生效。
pub fn save_config(app: &tauri::AppHandle, cfg: &AppConfig) -> anyhow::Result<()> {
    let dir = app.path().app_config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(CONFIG_FILE);
    std::fs::write(&path, serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}
