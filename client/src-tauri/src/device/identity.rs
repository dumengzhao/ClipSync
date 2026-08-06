//! 设备身份
//!
//! 每个设备拥有一对 X25519 身份密钥，私钥持久化于系统密钥链（keystore），
//! 不落明文盘。设备 ID 同样持久化，保证重启后身份稳定。

use anyhow::{anyhow, Result};
use rand::rngs::OsRng;
use uuid::Uuid;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::clipboard::types::DeviceId;
use crate::crypto::keystore;

const KEYSTORE_SERVICE: &str = "com.clipsync.device";
const ACCOUNT_DEVICE_ID: &str = "device-id";
const ACCOUNT_IDENTITY_KEY: &str = "identity-key";

/// 设备身份（含长期身份密钥对）
#[derive(Clone)]
pub struct DeviceIdentity {
    pub id: DeviceId,
    pub name: String,
    pub public_key: PublicKey,
    secret: StaticSecret,
}

impl DeviceIdentity {
    /// 加载已有身份，若不存在则生成并持久化。
    pub fn load_or_create(name: &str) -> Result<Self> {
        let id = match keystore::load(KEYSTORE_SERVICE, ACCOUNT_DEVICE_ID) {
            Ok(bytes) => DeviceId(
                String::from_utf8(bytes)
                    .map_err(|e| anyhow!("stored device id is not valid utf-8: {e}"))?,
            ),
            Err(_) => {
                let new_id = DeviceId(Uuid::new_v4().to_string());
                keystore::store(KEYSTORE_SERVICE, ACCOUNT_DEVICE_ID, new_id.0.as_bytes())
                    .map_err(anyhow::Error::msg)?;
                new_id
            }
        };

        let secret = match keystore::load(KEYSTORE_SERVICE, ACCOUNT_IDENTITY_KEY) {
            Ok(bytes) => {
                let arr: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| anyhow!("stored identity key has wrong length"))?;
                StaticSecret::from(arr)
            }
            Err(_) => {
                let s = StaticSecret::random_from_rng(OsRng);
                keystore::store(KEYSTORE_SERVICE, ACCOUNT_IDENTITY_KEY, s.as_bytes())
                    .map_err(anyhow::Error::msg)?;
                s
            }
        };

        let public_key = PublicKey::from(&secret);
        Ok(Self {
            id,
            name: name.to_string(),
            public_key,
            secret,
        })
    }

    /// 公钥字节（用于配对交换）
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.public_key.to_bytes()
    }

    /// 用于密钥协商的私钥引用
    pub fn secret(&self) -> &StaticSecret {
        &self.secret
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::keystore;

    #[test]
    fn identity_persists_across_loads() {
        let a = DeviceIdentity::load_or_create("test-device").unwrap();
        let b = DeviceIdentity::load_or_create("test-device").unwrap();
        // 同一进程内 keystore 已被第一次写入，第二次应读取到相同身份
        assert_eq!(a.id, b.id);
        assert_eq!(a.public_key_bytes(), b.public_key_bytes());

        // 清理测试写入的密钥链条目
        let _ = keystore::delete(KEYSTORE_SERVICE, ACCOUNT_DEVICE_ID);
        let _ = keystore::delete(KEYSTORE_SERVICE, ACCOUNT_IDENTITY_KEY);
    }
}
