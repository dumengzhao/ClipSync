//! SPAKE2 配对协议 - 基于 spake2 crate
//!
//! 配对码由发起方生成（6 位数字），两端使用相同配对码通过 SPAKE2 派生共享密钥。
//! 共享密钥可进一步经 HKDF（见 `crate::crypto::kdf`）派生会话密钥。
//!
//! 角色约定：发起方 = A，应答方 = B。两端必须使用相同的 idA / idB 标识串，
//! 以将密钥绑定到本服务，防止消息被重排到其他会话。

use crate::error::CryptoError;
use spake2::{Ed25519Group, Identity, Password, Spake2};

const ID_A: &[u8] = b"clipsync-initiator";
const ID_B: &[u8] = b"clipsync-responder";

/// 发起方（A 角色）配对会话
pub struct Initiator {
    state: Spake2<Ed25519Group>,
    pub message: Vec<u8>,
}

/// 应答方（B 角色）配对会话
pub struct Responder {
    state: Spake2<Ed25519Group>,
    pub message: Vec<u8>,
}

/// 生成 6 位数字配对码（用于 UI 展示）
pub fn generate_pairing_code() -> String {
    use rand::Rng;
    let n: u32 = rand::thread_rng().gen_range(100_000..1_000_000);
    format!("{n}")
}

/// 发起方：用配对码开始配对，返回首条待发送消息（33 字节）
pub fn start_initiator(password: &str) -> Initiator {
    let (state, message) = Spake2::<Ed25519Group>::start_a(
        &Password::new(password.as_bytes()),
        &Identity::new(ID_A),
        &Identity::new(ID_B),
    );
    Initiator { state, message }
}

/// 应答方：用配对码开始配对，返回首条待发送消息（33 字节）
pub fn start_responder(password: &str) -> Responder {
    let (state, message) = Spake2::<Ed25519Group>::start_b(
        &Password::new(password.as_bytes()),
        &Identity::new(ID_A),
        &Identity::new(ID_B),
    );
    Responder { state, message }
}

impl Initiator {
    /// 收到应答方消息后完成配对，派生 32 字节共享密钥
    pub fn finish(self, peer_message: &[u8]) -> Result<[u8; 32], CryptoError> {
        derive(self.state.finish(peer_message))
    }
}

impl Responder {
    /// 收到发起方消息后完成配对，派生 32 字节共享密钥
    pub fn finish(self, peer_message: &[u8]) -> Result<[u8; 32], CryptoError> {
        derive(self.state.finish(peer_message))
    }
}

fn derive(res: spake2::Result<Vec<u8>>) -> Result<[u8; 32], CryptoError> {
    let key = res.map_err(|e| CryptoError::InvalidKey(format!("{e:?}")))?;
    key.try_into()
        .map_err(|v: Vec<u8>| CryptoError::InvalidKey(format!("unexpected key length {}", v.len())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spake2_roundtrip_same_password() {
        let pw = "123456";
        let init = start_initiator(pw);
        let resp = start_responder(pw);
        let init_msg = init.message.clone();
        let k1 = init.finish(&resp.message).unwrap();
        let k2 = resp.finish(&init_msg).unwrap();
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 32);
    }

    #[test]
    fn spake2_wrong_password_mismatch() {
        let init = start_initiator("123456");
        let resp = start_responder("000000");
        let init_msg = init.message.clone();
        let k1 = init.finish(&resp.message).unwrap();
        let k2 = resp.finish(&init_msg).unwrap();
        // 错误配对码不会报错，但派生出的密钥不同
        assert_ne!(k1, k2);
    }

    #[test]
    fn pairing_code_is_six_digits() {
        let code = generate_pairing_code();
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }
}
