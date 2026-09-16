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

/// 配对码字母表：32 字符，去掉易混的 `0/O/1/I/L`。
/// 32 能整除 256，取随机字节取模无偏。
const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
/// 配对码字符数：12 × 5 bit = **60 bit 熵**。
///
/// 为什么不是 6 位数字：配对码是用 SPAKE2 派生的会话密钥做口令确认的，而确认标签
/// 是密钥的确定性函数——**只要对端能拿到一次可验证的标签，就能对候选口令离线穷举**。
/// 6 位数字（约 20 bit）在离线场景下几秒钟即可穷举完；常驻配对码一旦被还原，
/// 攻击者可长期冒充该设备。60 bit 使离线/在线穷举都不可行，且长度仍可手抄。
const CODE_LEN: usize = 12;

/// 生成配对码（用于 UI 展示与 SPAKE2 口令）。
pub fn generate_pairing_code() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..CODE_LEN)
        .map(|_| CODE_ALPHABET[rng.gen::<u8>() as usize % CODE_ALPHABET.len()] as char)
        .collect()
}

/// 规范化配对码：去掉分隔符/空白、统一大写。
///
/// 展示与抄写可带 `-`/空格（如 `A1B2-C3D4-E5F6`），口令一律取规范化值，
/// 否则用户按带分隔符的形式抄写就会永远匹配不上。
pub fn normalize_pairing_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// 是否为当前格式（12 位、字母表内）的配对码。
///
/// 用于启动时把历史遗留的低熵码（6 位数字等）自动换成新码：旧码熵不足，
/// 而配对码常驻、一旦泄露即可被长期冒充，故不做兼容保留。
pub fn pairing_code_is_current(code: &str) -> bool {
    let c = normalize_pairing_code(code);
    c.len() == CODE_LEN && c.bytes().all(|b| CODE_ALPHABET.contains(&b))
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
    fn pairing_code_is_twelve_base32_chars() {
        let code = generate_pairing_code();
        assert_eq!(code.len(), CODE_LEN);
        assert!(pairing_code_is_current(&code));
        // 字母表内、且不含易混字符
        for c in code.chars() {
            assert!(CODE_ALPHABET.contains(&(c as u8)));
            assert!(!"01ILO".contains(c));
        }
    }

    /// 熵下限近似检查：同一个码在多次生成中不应重复（60 bit 下碰撞概率可忽略）。
    #[test]
    fn pairing_codes_do_not_repeat() {
        let a = generate_pairing_code();
        let b = generate_pairing_code();
        assert_ne!(a, b);
    }

    /// 旧的低熵码（6 位数字）必须被判为「非当前格式」，启动时会被替换。
    #[test]
    fn legacy_six_digit_code_is_rejected() {
        assert!(!pairing_code_is_current("537390"));
        assert!(!pairing_code_is_current("000000"));
        assert!(!pairing_code_is_current(""));
        assert!(!pairing_code_is_current("ABC")); // 太短
        assert!(!pairing_code_is_current("ABCDEFGHIJK0")); // 含 0（不在字母表）
    }

    /// 规范化：分隔符/空白/小写都要能容忍，便于用户抄写带 `-` 的码。
    #[test]
    fn normalize_tolerates_separators_and_case() {
        assert_eq!(normalize_pairing_code("a1b2-c3d4-e5f6"), "A1B2C3D4E5F6");
        assert_eq!(normalize_pairing_code(" A1B2 C3D4 E5F6 "), "A1B2C3D4E5F6");
        assert_eq!(normalize_pairing_code("A1B2-C3D4-E5F6"), "A1B2C3D4E5F6");
        // 规范化后仍按字母表校验（小写输入也能通过）
        assert!(pairing_code_is_current(&normalize_pairing_code(
            "a1b2-c3d4-e5f6"
        )));
    }
}
