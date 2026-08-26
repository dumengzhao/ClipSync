//! HKDF 密钥派生
//!
//! 阶段一实现

use hkdf::Hkdf;
use sha2::Sha256;

/// 由 Network Token + Network ID 派生跨局域网文字中继的 AES-256 密钥。
///
/// 同一网络的所有设备用相同的 `(token, network_id)` 派生，得到同一把密钥，
/// 实现文字经服务端中继时的端到端加密（服务端只转发密文）。
pub fn derive_network_key(token: &str, network_id: &str) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, token.as_bytes());
    let mut key = [0u8; 32];
    // info 用 network_id，保证「换网络」后密钥不同（同一 Token 也能隔离）。
    hk.expand(network_id.as_bytes(), &mut key)
        .expect("hkdf expand to 32 bytes");
    key
}

pub fn derive_session_keys(
    shared_secret: &[u8],
    info: &[u8],
) -> crate::error::CryptoResult<[u8; 64]> {
    let hk = Hkdf::<Sha256>::new(None, shared_secret);
    let mut okm = [0u8; 64];
    hk.expand(info, &mut okm)
        .map_err(|e| crate::error::CryptoError::InvalidKey(e.to_string()))?;
    Ok(okm)
}

pub fn split_keys(okm: [u8; 64]) -> ([u8; 32], [u8; 32]) {
    let mut enc = [0u8; 32];
    let mut mac = [0u8; 32];
    enc.copy_from_slice(&okm[..32]);
    mac.copy_from_slice(&okm[32..]);
    (enc, mac)
}
