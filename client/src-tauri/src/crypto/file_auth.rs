//! 跨 LAN 文件拉取的请求方凭证。
//!
//! 背景：`GET /file/<hash>` 与同步服务共用监听端口，早期**只检查 hash 是否在本机
//! 登记过**，任何同网段主机拿到 hash 就能拉走文件（hash 不可预测是唯一防线）。
//! 这里补一道请求方鉴权：请求方必须证明自己持有同一网络密钥。
//!
//! 方案：`X-ClipSync-Auth: hex(HMAC-SHA256(network_key, "clipsync-file-v1" || hash))`。
//! - 密钥是跨 LAN 网络密钥（客户端与服务端共享、同网络各端一致），未连服务端的
//!   客户端没有密钥，自然无法构造合法凭证；
//! - 把上下文串 `clipsync-file-v1` 一起签进去，避免签名被挪用到其它用途；
//! - 校验用常量时间比较，防时序侧信道。
//!
//! ⚠️ 这是**协议变更**：两端需同步更新，旧客户端拉取会被 401 拒绝（这是预期行为）。

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// 签名上下文串（域分隔，防止签名被复用到其它用途）
const AUTH_CONTEXT: &[u8] = b"clipsync-file-v1";

/// 计算凭证（请求方调用）。
///
/// 不返回 `Result`：HMAC 接受任意长度密钥，`new_from_slice` 实际不会失败；
/// 万一失败则返回空串——空串在校验侧恒不匹配（`verify` 对空凭证直接判否），
/// 即失败方向偏安全，而不是 panic（项目规范：非测试代码禁止 `expect`）。
pub fn auth_token(network_key: &[u8; 32], hash: &str) -> String {
    let mut mac = match HmacSha256::new_from_slice(network_key) {
        Ok(m) => m,
        Err(_) => {
            tracing::error!("HMAC 初始化失败（理论不可达），凭证置空");
            return String::new();
        }
    };
    mac.update(AUTH_CONTEXT);
    mac.update(hash.as_bytes());
    to_hex(&mac.finalize().into_bytes())
}

/// 校验凭证（服务方调用）。空凭证直接判失败，比较为常量时间。
pub fn verify(network_key: &[u8; 32], hash: &str, presented: &str) -> bool {
    if presented.is_empty() {
        return false;
    }
    let expected = auth_token(network_key, hash);
    constant_time_eq(expected.as_bytes(), presented.trim().as_bytes())
}

/// 常量时间比较（长度不同直接返回 false——长度本身不是秘密）
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{auth_token, verify};

    #[test]
    fn round_trip() {
        let key = [7u8; 32];
        let t = auth_token(&key, "abc123");
        assert!(verify(&key, "abc123", &t));
        // 换 hash / 换 key / 空凭证 均失败
        assert!(!verify(&key, "other", &t));
        assert!(!verify(&[8u8; 32], "abc123", &t));
        assert!(!verify(&key, "abc123", ""));
        assert!(!verify(&key, "abc123", &t[..t.len() - 1]));
    }

    #[test]
    fn hex_is_lowercase_and_fixed_len() {
        assert_eq!(auth_token(&[0u8; 32], "x").len(), 64);
        assert!(auth_token(&[0u8; 32], "x")
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
