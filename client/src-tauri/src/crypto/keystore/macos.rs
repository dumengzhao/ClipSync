//! macOS 密钥存储：Keychain (security-framework)，不可用时回退到本地文件。
//!
//! 身份密钥与配对口令经系统 Keychain 加密存储，不落明文盘。
//!
//! 关键约束：macOS 的 `security-framework` 在**无 GUI 的后台会话**里调用
//! Keychain 时，会同步阻塞等待一个永远弹不出来的授权窗，直接卡死启动。
//! 因此：
//! - **发布构建（生产 .app）**：默认启用 Keychain，获得系统密钥链的安全保护。
//! - **调试构建（dev）**：默认改用本地文件存储，避免无界面/沙箱环境里的
//!   Keychain 授权弹窗与启动阻塞。可用 `CLIPSYNC_KEYCHAIN=1` 强制启用 Keychain
//!   以调试相关路径。

use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};
use std::io::{Read, Write};
use std::path::PathBuf;

const FALLBACK_DIR: &str = ".clipsync/keystore";

/// 是否启用系统 Keychain 存储。
fn keychain_enabled() -> bool {
    if std::env::var("CLIPSYNC_KEYCHAIN")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        return true;
    }
    #[cfg(debug_assertions)]
    {
        false
    }
    #[cfg(not(debug_assertions))]
    {
        true
    }
}

/// 计算回退文件路径：`~/.clipsync/keystore/<service>__<account>`
fn fallback_path(service: &str, account: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(FALLBACK_DIR)
        .join(format!("{service}__{account}"))
}

pub fn store(service: &str, account: &str, data: &[u8]) -> Result<(), String> {
    // 合并为一个条件表达式：clippy::collapsible_if（CI 的 -D warnings 会拒绝嵌套写法）
    if keychain_enabled() && set_generic_password(service, account, data).is_ok() {
        return Ok(());
    }
    file_store(service, account, data)
}

pub fn load(service: &str, account: &str) -> Result<Vec<u8>, String> {
    if keychain_enabled() {
        if let Ok(data) = get_generic_password(service, account) {
            return Ok(data);
        }
    }
    file_load(service, account)
}

pub fn delete(service: &str, account: &str) -> Result<(), String> {
    if keychain_enabled() && delete_generic_password(service, account).is_ok() {
        return Ok(());
    }
    file_delete(service, account)
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
    // device/store.rs 的降级口令文件已做同样处理（restrict_permissions），这里补齐。
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
