//! 设备身份
//!
//! 每个设备拥有一对 X25519 身份密钥，私钥持久化于系统密钥链（keystore），
//! 不落明文盘。设备 ID 由调用方解析后传入（解析规则见 [`resolve_device_id`]）。

use anyhow::{anyhow, Result};
use rand::rngs::OsRng;
use uuid::Uuid;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::clipboard::types::DeviceId;
use crate::crypto::keystore;

const KEYSTORE_SERVICE: &str = "com.clipsync.device";
const ACCOUNT_IDENTITY_KEY: &str = "identity-key";

/// 取不到机器码时生成的 fallback 设备 ID 前缀：
/// 用户可据此识别「该设备无法获取唯一机器码」。
const FALLBACK_ID_PREFIX: &str = "000000";
/// fallback 随机部分长度（hex 字符数）。
const FALLBACK_ID_RANDOM_LEN: usize = 12;

/// 解析设备 ID（纯函数，便于单测）。
///
/// 优先级：
/// 1. 配置中已有值 → 直接使用（首次解析后 config 即权威存储，终身不再重解析）；
/// 2. 机器码（`hw`）非空 → 使用机器码原值；
/// 3. 均不可得 → 生成 `000000` + 12 位随机 hex。
///
/// 返回 `(解析结果, 是否为新解析)`：`true` 时调用方须把结果写回配置落盘。
pub fn resolve_device_id(configured: &str, hw: &str) -> (String, bool) {
    let configured = configured.trim();
    if !configured.is_empty() {
        return (configured.to_string(), false);
    }
    let hw = hw.trim();
    if !hw.is_empty() {
        return (hw.to_string(), true);
    }
    let rand_part = Uuid::new_v4().simple().to_string()[..FALLBACK_ID_RANDOM_LEN].to_string();
    (format!("{FALLBACK_ID_PREFIX}{rand_part}"), true)
}

/// 设备身份（含长期身份密钥对）
#[derive(Clone)]
pub struct DeviceIdentity {
    pub id: DeviceId,
    pub name: String,
    pub public_key: PublicKey,
    secret: StaticSecret,
}

impl DeviceIdentity {
    /// 用已解析的设备 ID 与设备名构建身份；X25519 身份密钥从密钥链加载，不存在则生成并持久化。
    pub fn new(id: DeviceId, name: &str) -> Result<Self> {
        Self::new_in(KEYSTORE_SERVICE, id, name)
    }

    /// 指定 keystore service 的内部实现。
    /// 拆出来是为了让测试用独立的 service 名——测试若直接使用生产 service，
    /// 会在真实密钥链上读写、并在清理阶段把本机应用的真实设备身份删掉。
    fn new_in(service: &str, id: DeviceId, name: &str) -> Result<Self> {
        let secret = match keystore::load(service, ACCOUNT_IDENTITY_KEY) {
            Ok(bytes) => {
                let arr: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| anyhow!("stored identity key has wrong length"))?;
                StaticSecret::from(arr)
            }
            Err(_) => {
                let s = StaticSecret::random_from_rng(OsRng);
                // **持久化尽力而为**：keyring 不可用（无桌面会话 / CI / 容器）时不要让整个身份构建
                // 失败——那会让应用（以及依赖它的用例）直接起不来。失败只告警：本次会话用新密钥，
                // 设备身份以 config.device_id 为准，配对关系不受影响（仅身份公钥变化需重新握手）。
                if let Err(e) = keystore::store(service, ACCOUNT_IDENTITY_KEY, s.as_bytes()) {
                    tracing::warn!("身份密钥持久化失败（{e}）：本次会话使用临时密钥");
                }
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

    /// 测试专用 service：绝不能与生产 KEYSTORE_SERVICE 混用，
    /// 否则测试清理会把本机应用的真实设备身份从密钥链里删掉。
    const TEST_SERVICE: &str = "com.clipsync.device.test";

    #[test]
    fn identity_persists_across_loads() {
        let a = DeviceIdentity::new_in(TEST_SERVICE, DeviceId("test-id".into()), "test-device")
            .unwrap();
        let b = DeviceIdentity::new_in(TEST_SERVICE, DeviceId("test-id".into()), "test-device")
            .unwrap();
        // 同一进程内 keystore 已被第一次写入，第二次应读取到相同身份密钥
        assert_eq!(a.public_key_bytes(), b.public_key_bytes());

        // 清理测试写入的密钥链条目（仅测试 service，不影响生产数据）
        let _ = keystore::delete(TEST_SERVICE, ACCOUNT_IDENTITY_KEY);
    }

    #[test]
    fn resolve_prefers_configured_value() {
        let (id, fresh) = resolve_device_id("  existing-id  ", "some-machine-guid");
        assert_eq!(id, "existing-id");
        assert!(!fresh, "configured value must not be re-resolved");
    }

    #[test]
    fn resolve_falls_back_to_hardware_id() {
        let (id, fresh) = resolve_device_id("", "machine-guid-value");
        assert_eq!(id, "machine-guid-value");
        assert!(fresh, "hardware-derived id must be persisted");
    }

    #[test]
    fn resolve_generates_marked_fallback_when_no_hardware_id() {
        let (id, fresh) = resolve_device_id("", "");
        assert!(fresh);
        assert!(
            id.starts_with(FALLBACK_ID_PREFIX),
            "fallback id must carry the 000000 marker, got {id}"
        );
        // 000000 前缀 + 12 位随机 hex
        assert_eq!(id.len(), FALLBACK_ID_PREFIX.len() + FALLBACK_ID_RANDOM_LEN);
        assert!(
            id.chars()
                .skip(FALLBACK_ID_PREFIX.len())
                .all(|c| c.is_ascii_hexdigit()),
            "random part must be hex, got {id}"
        );
        // 每次生成都不同
        let (other, _) = resolve_device_id("", "");
        assert_ne!(id, other);
    }
}
