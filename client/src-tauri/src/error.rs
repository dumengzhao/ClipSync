//! 集中错误类型定义
//!
//! 对外 API 使用 thiserror 定义类型化错误，内部逻辑可用 anyhow

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("clipboard locked by another process")]
    Locked,
    #[error("unsupported content type: {0}")]
    UnsupportedType(String),
    #[error("content too large: {size} bytes (max {max})")]
    TooLarge { size: u64, max: u64 },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type ClipboardResult<T> = std::result::Result<T, ClipboardError>;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("encryption failed: {0}")]
    Encryption(String),
    #[error("decryption failed: {0}")]
    Decryption(String),
    #[error("invalid key: {0}")]
    InvalidKey(String),
    #[error("keystore error: {0}")]
    Keystore(String),
}

pub type CryptoResult<T> = std::result::Result<T, CryptoError>;

#[derive(Debug, Error)]
pub enum TransferError {
    #[error("connection closed")]
    ConnectionClosed,
    #[error("timeout after {0:?}")]
    Timeout(std::time::Duration),
    #[error("handshake failed: {0}")]
    Handshake(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type TransferResult<T> = std::result::Result<T, TransferError>;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("anti-loop detected: {0}")]
    AntiLoop(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("clipboard error: {0}")]
    Clipboard(#[from] ClipboardError),
    #[error("transfer error: {0}")]
    Transfer(#[from] TransferError),
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
}

pub type SyncResult<T> = std::result::Result<T, SyncError>;
