//! 加密模块
//!
//! - `aead`: AES-256-GCM 加密
//! - `kdf`: HKDF 密钥派生
//! - `pake`: SPAKE2 配对
//! - `keystore`: 平台密钥链封装

pub mod aead;
pub mod kdf;
pub mod keystore;
pub mod pake;
