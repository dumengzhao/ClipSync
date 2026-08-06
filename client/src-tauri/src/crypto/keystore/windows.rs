//! Windows 密钥存储：Credential Manager (Win32 CredWrite/CredRead)
//!
//! 身份密钥对经 Credential Manager 加密存储，不落明文盘。
//! 以 `service:account` 作为 Credential 的 TargetName 唯一标识。

use std::ptr;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Security::Credentials::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_FLAGS,
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
};

/// 构造 Credential 的 TargetName：`{service}:{account}`
fn target_name(service: &str, account: &str) -> String {
    format!("{service}:{account}")
}

pub fn store(service: &str, account: &str, data: &[u8]) -> Result<(), String> {
    let target = target_name(service, account);
    let target_w: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let user_w: Vec<u16> = account.encode_utf16().chain(std::iter::once(0)).collect();
    // CredentialBlob 必须指向稳定内存；用 Vec 持有并在 CredWrite 调用期间保持存活。
    // CredWrite 仅读取这些缓冲区，故以 const 指针转换传入即可，无需 mut。
    let blob: Vec<u8> = data.to_vec();

    let cred = CREDENTIALW {
        Flags: CRED_FLAGS(0),
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target_w.as_ptr() as *mut u16),
        Comment: PWSTR::null(),
        LastWritten: Default::default(),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_ptr() as *mut u8,
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        AttributeCount: 0,
        Attributes: ptr::null_mut(),
        TargetAlias: PWSTR::null(),
        UserName: PWSTR(user_w.as_ptr() as *mut u16),
    };

    unsafe { CredWriteW(&cred, 0).map_err(|e| format!("CredWrite failed: {e}")) }
}

pub fn load(service: &str, account: &str) -> Result<Vec<u8>, String> {
    let target = target_name(service, account);
    let target_w: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let mut pcred: *mut CREDENTIALW = ptr::null_mut();

    unsafe {
        CredReadW(PCWSTR(target_w.as_ptr()), CRED_TYPE_GENERIC, 0, &mut pcred)
            .map_err(|e| format!("CredRead failed: {e}"))?;

        let cred = *pcred;
        let blob = if cred.CredentialBlobSize > 0 {
            std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize)
                .to_vec()
        } else {
            Vec::new()
        };
        CredFree(pcred as *const std::ffi::c_void);
        Ok(blob)
    }
}

pub fn delete(service: &str, account: &str) -> Result<(), String> {
    let target = target_name(service, account);
    let target_w: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        CredDeleteW(PCWSTR(target_w.as_ptr()), CRED_TYPE_GENERIC, 0)
            .map_err(|e| format!("CredDelete failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_store_load() {
        let service = "com.clipsync.test";
        let account = "test-device-roundtrip";
        let secret = b"top-secret-identity-key-material";

        // 先清理可能残留的旧条目
        let _ = load(service, account);

        store(service, account, secret).expect("store should succeed");

        let loaded = load(service, account).expect("load should succeed");
        assert_eq!(loaded, secret);

        // 覆盖写入再次验证
        let secret2 = b"rotated-key";
        store(service, account, secret2).expect("overwrite should succeed");
        let loaded2 = load(service, account).expect("load after overwrite");
        assert_eq!(loaded2, secret2);
    }
}
