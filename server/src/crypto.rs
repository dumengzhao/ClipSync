use base64::Engine;
use rand::Rng;
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

/// 会话签名：HMAC-SHA256(server_key, payload)。服务端登录后下发，替代明文 admin_api_key。
pub fn sign_session(key: &str, payload: &str) -> String {
    use hmac::{Hmac, Mac};
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).expect("hmac key");
    mac.update(payload.as_bytes());
    let sig = mac.finalize().into_bytes();
    let mut out = payload.as_bytes().to_vec();
    out.push(b'.');
    out.extend_from_slice(&sig);
    base64::engine::general_purpose::STANDARD.encode(out)
}

/// 校验会话签名，成功返回 payload（JSON 字符串）。失败返回 None。
/// payload 形如 `{"user":"..","exp":..}`，此处只验签名，有效期由调用方检查。
pub fn verify_session(key: &str, token: &str) -> Option<String> {
    use hmac::{Hmac, Mac};
    type HmacSha256 = Hmac<Sha256>;
    let decoded = base64::engine::general_purpose::STANDARD.decode(token).ok()?;
    let dot = decoded.iter().position(|&b| b == b'.')?;
    let (payload, sig) = decoded.split_at(dot);
    let payload = std::str::from_utf8(&payload[..payload.len()]).ok()?;
    let sig = &sig[1..];
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).ok()?;
    mac.update(payload.as_bytes());
    mac.verify_slice(sig).ok()?;
    Some(payload.to_string())
}

/// 会话有效期（秒）：7 天。
pub const SESSION_TTL: i64 = 7 * 24 * 3600;
