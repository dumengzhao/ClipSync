//! macOS 密钥存储：Keychain (security-framework)
//!
//! 身份密钥对经系统 Keychain 加密存储，不落明文盘。
//! 以 `service` + `account` 唯一定位 Keychain 条目。

use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};

pub fn store(service: &str, account: &str, data: &[u8]) -> Result<(), String> {
    set_generic_password(service, account, data).map_err(|e| e.to_string())
}

pub fn load(service: &str, account: &str) -> Result<Vec<u8>, String> {
    get_generic_password(service, account).map_err(|e| e.to_string())
}

pub fn delete(service: &str, account: &str) -> Result<(), String> {
    delete_generic_password(service, account).map_err(|e| e.to_string())
}
