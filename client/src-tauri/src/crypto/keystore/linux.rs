//! Linux 密钥存储：Secret Service (libsecret via dbus-secret-service)，不可用时回退到本地文件。
//!
//! 身份密钥对优先经 Secret Service 加密存储，以 `service` / `account` 作为 item 的检索属性。
//!
//! **回退（与 macOS 侧同构，2026-09-18 补）**：很多 Linux 环境没有可用的 Secret Service
//! ——CI runner、无桌面的服务器、未解锁/未启动的 keyring、容器等，此时
//! `SecretService::connect` 会直接失败。原实现把失败当致命错误往上抛，导致
//! `DeviceIdentity::new` 整体失败（CI 上表现为 5 个用例 panic：`secret service connect failed:
//! DBus error: The name org.freedesktop.secrets was not provided by any .service files`）。
//! 现在：Secret Service 失败即改用 `~/.clipsync/keystore/<service>__<account>`（0600 权限），
//! 保证「无 keyring 也能持久化身份」，并在日志里明确告警（回退文件是明文，安全性低于 keyring）。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;

use dbus_secret_service::{EncryptionType, SecretService};

const FALLBACK_DIR: &str = ".clipsync/keystore";

/// 回退文件路径：`~/.clipsync/keystore/<service>__<account>`（与 macOS 侧同一布局）。
fn fallback_path(service: &str, account: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(FALLBACK_DIR)
        .join(format!("{service}__{account}"))
}

fn file_store(service: &str, account: &str, data: &[u8]) -> Result<(), String> {
    let p = fallback_path(service, account);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut f = std::fs::File::create(&p).map_err(|e| e.to_string())?;
    f.write_all(data).map_err(|e| e.to_string())?;
    // 权限收紧到「仅属主可读写」：回退文件里放的是身份私钥 / link secret / network_token，
    // 默认 umask（022）下同机其它用户可直接读取——那等于密钥明文泄露。
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!("收紧回退密钥文件权限失败 {:?}: {e}", p);
        }
    }
    Ok(())
}

fn file_load(service: &str, account: &str) -> Result<Vec<u8>, String> {
    let p = fallback_path(service, account);
    let mut f = std::fs::File::open(&p).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

fn file_delete(service: &str, account: &str) -> Result<(), String> {
    let p = fallback_path(service, account);
    std::fs::remove_file(&p).map_err(|e| e.to_string())?;
    Ok(())
}

/// 首次因 keyring 不可用而回退时告警一次（避免每次读写都刷日志）。
static FALLBACK_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn warn_fallback_once(err: &str) {
    if !FALLBACK_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            "Secret Service 不可用（{err}），密钥回退到 ~/{FALLBACK_DIR}/<service>__<account>（0600，明文）。             无桌面会话 / keyring 未启动 / 容器环境属正常情况。"
        );
    }
}

pub fn store(service: &str, account: &str, data: &[u8]) -> Result<(), String> {
    if let Err(e) = secret_service_store(service, account, data) {
        warn_fallback_once(&e);
        return file_store(service, account, data);
    }
    Ok(())
}

/// 走 Secret Service 写入（失败由 `store` 决定是否回退）。
fn secret_service_store(service: &str, account: &str, data: &[u8]) -> Result<(), String> {
    let service_api = SecretService::connect(EncryptionType::Dh)
        .map_err(|e| format!("secret service connect failed: {e}"))?;
    let collection = service_api
        .get_default_collection()
        .map_err(|e| format!("get default collection failed: {e}"))?;

    let mut attributes = HashMap::new();
    attributes.insert("service", service);
    attributes.insert("account", account);

    collection
        .create_item(account, attributes, data, true, "application/octet-stream")
        .map_err(|e| format!("create item failed: {e}"))?;

    Ok(())
}

pub fn load(service: &str, account: &str) -> Result<Vec<u8>, String> {
    match secret_service_load(service, account) {
        Ok(v) => Ok(v),
        Err(e) => {
            // 先试 keyring，再看回退文件；两处都没有才算「没存过」（调用方据此生成新密钥）
            match file_load(service, account) {
                Ok(v) => Ok(v),
                Err(_) => {
                    warn_fallback_once(&e);
                    Err(e)
                }
            }
        }
    }
}

/// 走 Secret Service 读取。
fn secret_service_load(service: &str, account: &str) -> Result<Vec<u8>, String> {
    let service_api = SecretService::connect(EncryptionType::Dh)
        .map_err(|e| format!("secret service connect failed: {e}"))?;
    let collection = service_api
        .get_default_collection()
        .map_err(|e| format!("get default collection failed: {e}"))?;

    let mut attributes = HashMap::new();
    attributes.insert("service", service);
    attributes.insert("account", account);

    let items = collection
        .search_items(attributes)
        .map_err(|e| format!("search items failed: {e}"))?;

    let item = items
        .into_iter()
        .next()
        .ok_or_else(|| "no stored credential found".to_string())?;

    item.get_secret()
        .map_err(|e| format!("get secret failed: {e}"))
}

pub fn delete(service: &str, account: &str) -> Result<(), String> {
    // 两处都要清：keyring 能连就连，连不上时至少清掉回退文件，避免「删了又回来」。
    let ss = secret_service_delete(service, account);
    let file = file_delete(service, account);
    match (ss, file) {
        (Ok(_), _) => Ok(()),
        // keyring 不可用（正常情况）且没有回退文件 → 视为已删除
        (Err(_), Err(e)) if e.contains("No such file") => Ok(()),
        (Err(e), _) => Err(e),
    }
}

/// 走 Secret Service 删除。
fn secret_service_delete(service: &str, account: &str) -> Result<(), String> {
    let service_api = SecretService::connect(EncryptionType::Dh)
        .map_err(|e| format!("secret service connect failed: {e}"))?;
    let collection = service_api
        .get_default_collection()
        .map_err(|e| format!("get default collection failed: {e}"))?;

    let mut attributes = HashMap::new();
    attributes.insert("service", service);
    attributes.insert("account", account);

    let items = collection
        .search_items(attributes)
        .map_err(|e| format!("search items failed: {e}"))?;

    for item in items {
        item.delete()
            .map_err(|e| format!("delete item failed: {e}"))?;
    }
    Ok(())
}
