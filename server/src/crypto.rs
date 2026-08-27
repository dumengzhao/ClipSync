use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 签发管理员会话 JWT（HS256，密钥为 server.key）。
/// 完全无状态：令牌自包含 exp，服务端不存储任何会话。
pub fn issue_session(key: &str, user: &str) -> String {
    let now = now_unix() as usize;
    let claims = Claims {
        sub: user.to_string(),
        iat: now,
        exp: now + SESSION_TTL as usize,
    };
    let enc = jsonwebtoken::EncodingKey::from_secret(key.as_bytes());
    jsonwebtoken::encode(&jsonwebtoken::Header::default(), &claims, &enc)
        .expect("jwt encode")
}

/// 校验管理员会话 JWT（验签名 + 验 exp，由 jsonwebtoken 库保证）。
/// 成功返回用户名（sub），失败返回 None。
pub fn verify_session(key: &str, token: &str) -> Option<String> {
    let dec = jsonwebtoken::DecodingKey::from_secret(key.as_bytes());
    let validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    match jsonwebtoken::decode::<Claims>(token, &dec, &validation) {
        Ok(data) => Some(data.claims.sub),
        Err(_) => None,
    }
}

/// 会话有效期（秒）：7 天。
pub const SESSION_TTL: i64 = 7 * 24 * 3600;
