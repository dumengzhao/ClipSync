//! 配对信息持久化
//!
//! 配对成功后必须能跨重启保留，否则每次启动都要重新走一遍交互配对。
//! 数据分两部分落地，敏感程度不同、载体也不同：
//!
//! - **设备元数据**（id / 名称 / 指纹 / 信任级别 / 最后在线）→ 明文 JSON，
//!   位于 `<home_dir>/ClipSync/paired_devices.json`（用户主目录下、非隐藏）。
//!   这些信息本就要展示给用户，不敏感。
//! - **重连口令（link secret）** → 系统密钥链（Windows Credential Manager /
//!   macOS Keychain / Linux Secret Service）。它等价于长期共享密钥，泄露即可
//!   冒充已配对设备，绝不能明文落盘。
//!
//! 密钥链在部分环境不可用（典型：无 Secret Service 的 headless Linux）。此时
//! 降级写入 `<home_dir>/ClipSync/paired_secrets.json` 并收紧到属主可读写（0o600），同时打
//! WARN——**能用**优先于**完美**，但要让用户知道降级发生了。

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::clipboard::types::DeviceId;
use crate::crypto::keystore;
use crate::device::registry::{PairedDevice, TrustLevel};

const DEVICES_FILE: &str = "paired_devices.json";
const SECRETS_FILE: &str = "paired_secrets.json";
const KEYSTORE_SERVICE: &str = "com.clipsync.pairing";

/// 磁盘上的已配对设备记录。字段全部带 `#[serde(default)]`，
/// 保证旧版本写下的文件在新增字段后仍能读出来，而不是整个配对表失效。
#[derive(Serialize, Deserialize, Clone)]
pub struct PairedRecord {
    pub device_id: String,
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub last_seen: u64,
    /// 最后成功连接的对端可拨号地址（host:port），mDNS 失效时兜底重连用。
    #[serde(default)]
    pub last_addr: Option<String>,
}

impl From<&PairedDevice> for PairedRecord {
    fn from(d: &PairedDevice) -> Self {
        Self {
            device_id: d.device_id.0.clone(),
            device_name: d.device_name.clone(),
            fingerprint: d.fingerprint.clone(),
            verified: matches!(d.trust, TrustLevel::Verified),
            last_seen: d.last_seen,
            last_addr: d.last_addr.clone(),
        }
    }
}

impl From<PairedRecord> for PairedDevice {
    fn from(r: PairedRecord) -> Self {
        Self {
            device_id: DeviceId(r.device_id),
            device_name: r.device_name,
            fingerprint: r.fingerprint,
            trust: if r.verified {
                TrustLevel::Verified
            } else {
                TrustLevel::Unverified
            },
            last_seen: r.last_seen,
            last_addr: r.last_addr,
        }
    }
}

fn config_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    crate::config::clipsync_base_dir(app)
}

fn devices_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    config_dir(app).map(|d| d.join(DEVICES_FILE))
}

fn secrets_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    config_dir(app).map(|d| d.join(SECRETS_FILE))
}

/// 升级迁移：将旧位置（应用配置目录 `<app_config_dir>`）下的配对文件一次性搬到
/// 新的用户主目录 ClipSync/ 位置（仅一次）。保证升级/重装后历史配对关系与降级口令不丢失；
/// 新位置已存在对应文件或旧位置无文件则跳过。口令文件迁移后收紧到属主可读写。
fn migrate_legacy_files(app: &tauri::AppHandle) {
    let Some(new_dir) = crate::config::clipsync_base_dir(app) else {
        return;
    };
    let Some(old_dir) = app.path().app_config_dir().ok() else {
        return;
    };
    if std::fs::create_dir_all(&new_dir).is_err() {
        tracing::warn!("创建配置目录失败，跳过配对文件迁移");
        return;
    }
    for fname in [DEVICES_FILE, SECRETS_FILE] {
        let old = old_dir.join(fname);
        let new = new_dir.join(fname);
        if new.exists() || !old.exists() {
            continue;
        }
        match std::fs::copy(&old, &new) {
            Ok(_) => {
                tracing::info!("已从旧位置迁移配对文件 {fname} 到 {}", new.display());
                if fname == SECRETS_FILE {
                    restrict_permissions(&new);
                }
            }
            Err(e) => tracing::warn!("迁移配对文件 {fname} 失败（不影响启动）：{e}"),
        }
    }
}

/// 读取已配对设备列表。文件缺失或损坏时返回空表（视为「尚未配对过」），不报错中断启动。
pub fn load_devices(app: &tauri::AppHandle) -> Vec<PairedDevice> {
    migrate_legacy_files(app);
    let Some(path) = devices_path(app) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str::<Vec<PairedRecord>>(&text) {
        Ok(list) => list.into_iter().map(PairedDevice::from).collect(),
        Err(e) => {
            tracing::warn!("已配对设备表解析失败（将视为空表）：{e}");
            Vec::new()
        }
    }
}

/// 覆盖写入已配对设备列表。
pub fn save_devices(app: &tauri::AppHandle, devices: &[PairedDevice]) {
    let Some(dir) = config_dir(app) else {
        tracing::warn!("无法定位配置目录，已配对设备表未能保存");
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("创建配置目录失败，已配对设备表未能保存：{e}");
        return;
    }
    let records: Vec<PairedRecord> = devices.iter().map(PairedRecord::from).collect();
    match serde_json::to_string_pretty(&records) {
        Ok(text) => {
            if let Err(e) = std::fs::write(dir.join(DEVICES_FILE), text) {
                tracing::warn!("写入已配对设备表失败：{e}");
            }
        }
        Err(e) => tracing::warn!("序列化已配对设备表失败：{e}"),
    }
}

/// 保存某对端的重连口令。优先系统密钥链，不可用时降级到受限权限的本地文件。
pub fn store_secret(app: &tauri::AppHandle, device_id: &str, secret: &str) {
    match keystore::store(KEYSTORE_SERVICE, device_id, secret.as_bytes()) {
        Ok(()) => {
            // 密钥链可用，清掉可能存在的降级副本，避免同一秘密两处留存
            remove_fallback_secret(app, device_id);
        }
        Err(e) => {
            tracing::warn!("密钥链不可用（{e}），配对口令降级保存到本地文件");
            let mut map = read_fallback_secrets(app);
            map.insert(device_id.to_string(), secret.to_string());
            write_fallback_secrets(app, &map);
        }
    }
}

/// 读取某对端的重连口令；密钥链没有时回落到降级文件。
pub fn load_secret(app: &tauri::AppHandle, device_id: &str) -> Option<String> {
    if let Ok(bytes) = keystore::load(KEYSTORE_SERVICE, device_id) {
        if let Ok(s) = String::from_utf8(bytes) {
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    read_fallback_secrets(app).get(device_id).cloned()
}

/// 删除某对端的重连口令（取消配对时调用），两处载体都要清。
pub fn delete_secret(app: &tauri::AppHandle, device_id: &str) {
    let _ = keystore::delete(KEYSTORE_SERVICE, device_id);
    remove_fallback_secret(app, device_id);
}

fn remove_fallback_secret(app: &tauri::AppHandle, device_id: &str) {
    let mut map = read_fallback_secrets(app);
    if map.remove(device_id).is_some() {
        write_fallback_secrets(app, &map);
    }
}

fn read_fallback_secrets(app: &tauri::AppHandle) -> HashMap<String, String> {
    migrate_legacy_files(app);
    secrets_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_fallback_secrets(app: &tauri::AppHandle, map: &HashMap<String, String>) {
    let Some(dir) = config_dir(app) else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(SECRETS_FILE);
    let Ok(text) = serde_json::to_string_pretty(map) else {
        return;
    };
    if let Err(e) = std::fs::write(&path, text) {
        tracing::warn!("写入降级配对口令文件失败：{e}");
        return;
    }
    restrict_permissions(&path);
}

/// 把降级口令文件收紧到「仅属主可读写」。Windows 上依赖用户目录本身的 ACL，无需额外处理。
#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!("收紧配对口令文件权限失败：{e}");
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips_through_paired_device() {
        let original = PairedDevice {
            device_id: DeviceId("dev-1".into()),
            device_name: "笔记本".into(),
            fingerprint: "abcdef".into(),
            trust: TrustLevel::Verified,
            last_seen: 42,
            last_addr: Some("192.168.1.50:24681".into()),
        };
        let back: PairedDevice = PairedRecord::from(&original).into();
        assert_eq!(back.device_id, original.device_id);
        assert_eq!(back.device_name, original.device_name);
        assert_eq!(back.fingerprint, original.fingerprint);
        assert_eq!(back.trust, original.trust);
        assert_eq!(back.last_seen, original.last_seen);
        assert_eq!(back.last_addr, original.last_addr);
    }

    /// 旧版本写下的文件缺少后加的字段时，必须仍能读出来，
    /// 否则一次格式升级就会让用户所有配对全部失效。
    #[test]
    fn record_tolerates_missing_fields() {
        let r: PairedRecord = serde_json::from_str(r#"{"device_id":"only-id"}"#).unwrap();
        assert_eq!(r.device_id, "only-id");
        assert!(!r.verified);
        assert_eq!(r.last_seen, 0);
    }
}
