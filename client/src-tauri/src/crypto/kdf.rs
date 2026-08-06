//! HKDF 密钥派生
//!
//! 阶段一实现

use hkdf::Hkdf;
use sha2::Sha256;

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
