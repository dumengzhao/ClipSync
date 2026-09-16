//! 配置模块

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
                // 建目录失败必须留痕：它会让下面的 copy 必然失败，进而决定
                // 「旧文件能不能删」（见 scrub_legacy_config 的前置条件）。
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!("创建配置目录失败 {:?}: {e}", parent);
                }
            }
            match std::fs::copy(&old, new_path) {
                Ok(_) => {
                    tracing::info!("已从旧位置迁移配置到 {}", new_path.display());
                    // 迁移成功后删掉旧文件：新配置已不再落明文 network_token，
                    // 旧副本若留着就是给「密钥链加固」留了个后门。
                    if let Err(e) = std::fs::remove_file(&old) {
                        tracing::warn!("删除旧配置文件失败（其中可能残留明文网络密钥）: {e}");
                    }
                }
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
    // 旧位置的配置文件可能含着加固前的**明文** network_token。
    // 新配置已改为只存系统密钥链（落盘置空），但只要旧副本还在磁盘上，
    // 这次加固就等于白做（实测确认过：迁移完成后旧文件仍在且带明文）。这里清掉。
    // 注意：仅在迁移确实产出新配置时才清理（见该函数内的前置条件）。
    scrub_legacy_config(app, &path);
    let mut cfg = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("config parse failed ({}), falling back to defaults", e);
            AppConfig::default()
        }),
        Err(_) => AppConfig::default(),
    };
    cfg = merge_network_token(app, cfg);
    migrate_device_name(app, cfg)
}

/// 网络密钥（`network_token`）的存取：**优先放系统密钥链**，不落配置文件。
///
/// 它是跨 LAN 的鉴权凭证兼端到端加密密钥，等同口令；早期版本直接明文写在
/// `config.json` 里，同机其它用户 / 备份 / 同步目录都能读到。
/// 兼容策略：密钥链不可用（如没有 Secret Service 的 Linux）时，`save_config`
/// 会回退明文落盘，此时这里就以文件里的值为准——不因加固而丢掉用户配置。
const KEYRING_SERVICE: &str = "com.clipsync.network";
const KEYRING_ACCOUNT_TOKEN: &str = "network_token";

/// 读取时把密钥链里的网络密钥合并回来；若配置文件里还留着老的明文 token，
/// 顺手迁移进密钥链并重写配置（抹掉明文）。
fn merge_network_token(app: &tauri::AppHandle, mut cfg: AppConfig) -> AppConfig {
    let from_keyring = crate::crypto::keystore::load(KEYRING_SERVICE, KEYRING_ACCOUNT_TOKEN)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|t| !t.is_empty());
    match from_keyring {
        Some(t) => cfg.network_token = t,
        // 密钥链里没有、但配置文件里有（老版本遗留的明文）：迁移进密钥链后抹掉明文
        None if !cfg.network_token.is_empty()
            && crate::crypto::keystore::store(
                KEYRING_SERVICE,
                KEYRING_ACCOUNT_TOKEN,
                cfg.network_token.as_bytes(),
            )
            .is_ok() =>
        {
            if let Err(e) = save_config(app, &cfg) {
                tracing::warn!("迁移网络密钥后重写配置失败（明文暂留）: {e}");
            } else {
                tracing::info!("network_token 已从配置文件迁移进系统密钥链");
            }
        }
        None => {}
    }
    cfg
}

/// 清理旧位置（应用配置目录）的配置文件。
///
/// 加固前 network_token 是明文写在配置里的；改用系统密钥链后，新配置落盘时该字段为空，
/// 但**旧路径的副本**依旧带明文——实测发现「迁移只 copy 不删源」会让明文长期躺着。
/// 因此：优先整体删除（新路径已是权威源）；删不掉则退而抹掉其中的 token 字段。
///
/// **前置条件（必须保留）**：只有在新路径的配置文件**确实存在**时才允许动旧文件。
/// 早期版本无条件清理，于是「迁移 copy 失败（磁盘满 / 权限 / 建目录失败）」时，
/// 旧文件是用户配置的唯一副本，却被照删 → 服务端地址、Token、手动地址、配对码
/// 全部静默丢失、直接回落到默认配置。宁可留一份带明文的旧副本（有 WARN 可查），
/// 也不能丢掉用户的配置。
fn scrub_legacy_config(app: &tauri::AppHandle, new_path: &std::path::Path) {
    let Some(old) = app
        .path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join(CONFIG_FILE))
    else {
        return;
    };
    // 没有旧文件就没有可清理的东西（全新安装的正常路径，不告警）
    if !old.exists() {
        return;
    }
    // 新路径与旧路径可能是同一个文件（home_dir 与 app_config_dir 重合的极端环境）：
    // 那样「清理旧文件」等于删掉权威配置本身。
    if old == new_path {
        return;
    }
    // 核心前置条件：新文件不存在说明迁移没成功，旧文件是配置的唯一副本，不能删。
    if !new_path.exists() {
        tracing::warn!(
            "新配置 {} 不存在（迁移未成功），保留旧位置配置文件不清理",
            new_path.display()
        );
        return;
    }
    if std::fs::remove_file(&old).is_ok() {
        tracing::info!(
            "已删除旧位置配置文件（避免残留明文网络密钥）: {}",
            old.display()
        );
        return;
    }
    // 删除失败（占用/权限）：至少把其中的明文 token 抹掉
    if let Ok(text) = std::fs::read_to_string(&old) {
        if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) {
            let has_plain = v
                .get("network_token")
                .and_then(|t| t.as_str())
                .is_some_and(|s| !s.is_empty());
            if has_plain {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "network_token".to_string(),
                        serde_json::Value::String(String::new()),
                    );
                }
                if let Ok(s) = serde_json::to_string_pretty(&v) {
                    let _ = std::fs::write(&old, s);
                    tracing::warn!(
                        "旧配置文件无法删除，已抹除其中的明文网络密钥: {}",
                        old.display()
                    );
                }
            }
        }
    }
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
    let path =
        config_path(app).ok_or_else(|| anyhow::anyhow!("无法确定用户主目录，无法写入配置"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 网络密钥不进配置文件：优先写系统密钥链，成功则落盘时置空。
    // 密钥链不可用时保持明文落盘（记录警告），避免 Linux 等环境下重启丢 token。
    let mut to_write = cfg.clone();
    if cfg.network_token.is_empty() {
        // 用户清空了 token：同步清掉密钥链里的副本
        let _ = crate::crypto::keystore::delete(KEYRING_SERVICE, KEYRING_ACCOUNT_TOKEN);
    } else {
        match crate::crypto::keystore::store(
            KEYRING_SERVICE,
            KEYRING_ACCOUNT_TOKEN,
            cfg.network_token.as_bytes(),
        ) {
            Ok(()) => to_write.network_token = String::new(),
            Err(e) => tracing::warn!("网络密钥写入系统密钥链失败，改为随配置明文落盘: {e}"),
        }
    }
    std::fs::write(&path, serde_json::to_string_pretty(&to_write)?)?;
    Ok(())
}
