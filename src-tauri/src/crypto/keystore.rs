//! 平台密钥链封装
//!
//! - macOS: Keychain (security-framework)
//! - Windows: DPAPI (windows crate)
//! - Linux: Secret Service (dbus-secret-service)

#[cfg(target_os = "macos")]
pub mod platform {
    use security_framework::passwords::set_generic_password;
    use security_framework::passwords::get_generic_password;

    pub fn store(service: &str, account: &str, data: &[u8]) -> Result<(), String> {
        set_generic_password(service, account, data).map_err(|e| e.to_string())
    }

    pub fn load(service: &str, account: &str) -> Result<Vec<u8>, String> {
        get_generic_password(service, account).map_err(|e| e.to_string())
    }
}

#[cfg(target_os = "windows")]
pub mod platform {
    // TODO: 阶段一实现 - DPAPI
    pub fn store(_service: &str, _account: &str, _data: &[u8]) -> Result<(), String> {
        Err("not yet implemented".to_string())
    }
    pub fn load(_service: &str, _account: &str) -> Result<Vec<u8>, String> {
        Err("not yet implemented".to_string())
    }
}

#[cfg(target_os = "linux")]
pub mod platform {
    // TODO: 阶段一实现 - Secret Service
    pub fn store(_service: &str, _account: &str, _data: &[u8]) -> Result<(), String> {
        Err("not yet implemented".to_string())
    }
    pub fn load(_service: &str, _account: &str) -> Result<Vec<u8>, String> {
        Err("not yet implemented".to_string())
    }
}

pub use platform::{load, store};
