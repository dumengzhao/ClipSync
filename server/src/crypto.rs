use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Token → SHA-256 哈希（服务端只存哈希，用于连接校验）。
pub fn hash_token(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

/// 生成随机 Token / Network ID 等（24 字节 hex）。
pub fn gen_token() -> String {
    let mut rng = rand::thread_rng();
    let mut bytes = [0u8; 24];
    rng.fill(&mut bytes);
    hex::encode(bytes)
}

/// 管理员会话 Claims（标准 JWT）。
#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    /// 主题：管理员用户名
    sub: String,
    /// 签发时间（Unix 秒）
    iat: usize,
    /// 过期时间（Unix 秒）
    exp: usize,
}

#[derive(Serialize)]
struct JwtHeader {
    alg: &'static str,
    typ: &'static str,
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn b64url_encode(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s).ok()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac key");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// 常量时间比较（HMAC-SHA256 输出固定 32 字节）。
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 会话有效期（秒）：3 天。
pub const SESSION_TTL: i64 = 3 * 24 * 3600;

/// 签发管理员会话 JWT（自实现 HS256，密钥为 server.key）。
/// 完全无状态：令牌自包含 exp，服务端不存储任何会话。
/// 不依赖 ring / 任何 C 工具链，可纯 Rust 交叉编译到任意平台。
pub fn issue_session(key: &str, user: &str) -> String {
    let now = now_unix() as usize;
    let claims = Claims {
        sub: user.to_string(),
        iat: now,
        exp: now + SESSION_TTL as usize,
    };
    let header_json = serde_json::to_string(&JwtHeader { alg: "HS256", typ: "JWT" }).expect("jwt header");
    let payload_json = serde_json::to_string(&claims).expect("jwt payload");
    let header_b64 = b64url_encode(header_json.as_bytes());
    let payload_b64 = b64url_encode(payload_json.as_bytes());
    let signing_input = format!("{}.{}", header_b64, payload_b64);
    let sig = hmac_sha256(key.as_bytes(), signing_input.as_bytes());
    let sig_b64 = b64url_encode(&sig);
    format!("{}.{}", signing_input, sig_b64)
}

/// 校验管理员会话 JWT（验签名 + 验 exp）。成功返回用户名（sub），失败返回 None。
pub fn verify_session(key: &str, token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let expected = hmac_sha256(key.as_bytes(), signing_input.as_bytes());
    let got = b64url_decode(parts[2])?;
    if !ct_eq(&expected, &got) {
        return None;
    }
    let payload = b64url_decode(parts[1])?;
    let claims: Claims = serde_json::from_slice(&payload).ok()?;
    if claims.exp as i64 <= now_unix() {
        return None;
    }
    Some(claims.sub)
}
