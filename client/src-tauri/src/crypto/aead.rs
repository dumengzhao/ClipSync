//! AES-256-GCM 加密
//!
//! 阶段一实现

use aes_gcm::aead::{Aead, AeadInPlace, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};

pub const NONCE_SIZE: usize = 12;
pub const KEY_SIZE: usize = 32;
/// AES-GCM 认证标签长度（`decrypt_in_place` 需要预留）
const TAG_SIZE: usize = 16;

pub struct EncryptedMessage {
    pub nonce: [u8; NONCE_SIZE],
    pub ciphertext: Vec<u8>,
    pub hmac: [u8; 32],
}

pub fn encrypt(
    key: &[u8; KEY_SIZE],
    nonce: &[u8; NONCE_SIZE],
    plaintext: &[u8],
) -> crate::error::CryptoResult<Vec<u8>> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .encrypt(Nonce::from_slice(nonce), plaintext)
        .map_err(|e| crate::error::CryptoError::Encryption(e.to_string()))
}

pub fn decrypt(
    key: &[u8; KEY_SIZE],
    nonce: &[u8; NONCE_SIZE],
    ciphertext: &[u8],
) -> crate::error::CryptoResult<Vec<u8>> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|e| crate::error::CryptoError::Decryption(e.to_string()))
}

/// 原地解密：成功后 `buf` 内容被替换为明文（长度减去认证标签）。
///
/// 用途是**降低大文件解密的内存峰值**——`decrypt` 会同时持有密文与明文两份
/// （≈2× 文件大小），而这里密文缓冲区被复用，峰值降到 ≈1×。
/// 失败时 `buf` 内容未定义，调用方必须丢弃。
pub fn decrypt_in_place(
    key: &[u8; KEY_SIZE],
    nonce: &[u8; NONCE_SIZE],
    buf: &mut Vec<u8>,
) -> crate::error::CryptoResult<()> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt_in_place(Nonce::from_slice(nonce), b"", buf)
        .map_err(|e| crate::error::CryptoError::Decryption(e.to_string()))
}

/// `decrypt_in_place` 要求缓冲区至少这么长（认证标签本身的开销）
pub const fn in_place_overhead() -> usize {
    TAG_SIZE
}
