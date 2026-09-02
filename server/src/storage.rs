use crate::models::Network;
use anyhow::Result;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::rngs::OsRng;
use std::path::{Path, PathBuf};

/// 文件存储（无数据库）：networks.json / server.key
/// 管理员凭据不落盘——单一来源为环境变量 ADMIN_USER / ADMIN_PASS。
pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        std::fs::create_dir_all(&dir).ok();
        Store { dir }
    }
    fn networks_path(&self) -> PathBuf {
        self.dir.join("networks.json")
    }
    fn key_path(&self) -> PathBuf {
        self.dir.join("server.key")
    }

    pub fn load_networks(&self) -> Vec<Network> {
        let p = self.networks_path();
        match std::fs::read_to_string(&p) {
            Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).unwrap_or_default(),
            _ => Vec::new(),
        }
    }
    pub fn save_networks(&self, nets: &[Network]) -> Result<()> {
        let s = serde_json::to_string_pretty(nets)?;
        atomic_write(&self.networks_path(), &s)
    }

    /// 读取会话签名密钥；缺失则随机生成并落盘。
    pub fn load_or_create_key(&self) -> Result<String> {
        let p = self.key_path();
        if let Ok(s) = std::fs::read_to_string(&p) {
            let t = s.trim().to_string();
            if !t.is_empty() {
                return Ok(t);
            }
        }
        let key = crate::crypto::gen_token();
        atomic_write(&p, &key)?;
        Ok(key)
    }
}

/// 原子写：写临时文件 + rename，避免半截文件。
pub fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn hash_pass(pass: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pass.as_bytes(), &salt)
        .unwrap()
        .to_string()
}

pub fn verify_pass(hash: &str, pass: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(pass.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}
