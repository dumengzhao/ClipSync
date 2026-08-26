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
