//! AES-256-GCM 加密
//!
//! 阶段一实现

use aes_gcm::{Aes256Gcm, Key, Nonce};
use aes_gcm::aead::{Aead, KeyInit};

pub const NONCE_SIZE: usize = 12;
pub const KEY_SIZE: usize = 32;

pub struct EncryptedMessage {
    pub nonce: [u8; NONCE_SIZE],
    pub ciphertext: Vec<u8>,
    pub hmac: [u8; 32],
}

pub fn encrypt(key: &[u8; KEY_SIZE], nonce: &[u8; NONCE_SIZE], plaintext: &[u8]) -> crate::error::CryptoResult<Vec<u8>> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .encrypt(Nonce::from_slice(nonce), plaintext)
        .map_err(|e| crate::error::CryptoError::Encryption(e.to_string()))
}

pub fn decrypt(key: &[u8; KEY_SIZE], nonce: &[u8; NONCE_SIZE], ciphertext: &[u8]) -> crate::error::CryptoResult<Vec<u8>> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|e| crate::error::CryptoError::Decryption(e.to_string()))
}
