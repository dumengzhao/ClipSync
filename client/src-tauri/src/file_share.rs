//! 跨 LAN 文件共享注册表
//!
//! 本机复制文件时，把「文件内容哈希 → 本地路径」登记进来；对端经本机对外地址
//! `ext_file_ep/file/<hash>` 拉取时，按 hash 取回字节。服务端只转发 manifest，不碰字节。

use crate::clipboard::types::FileMeta;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use sha2::{Digest, Sha256};

#[derive(Default)]
pub struct FileShare {
    /// hash -> 本机文件路径（仅本网络已复制且仍存在的文件）
    map: Mutex<HashMap<String, PathBuf>>,
}

impl FileShare {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册本地文件，返回可下发的 manifest（Vec<FileMeta>，含内容哈希）。
    pub fn register(&self, paths: &[PathBuf]) -> serde_json::Value {
        let mut out = Vec::new();
        for p in paths {
            if let Some(m) = build_meta(p) {
                if let Some(h) = &m.hash {
                    self.map.lock().unwrap().insert(h.clone(), p.clone());
                }
                out.push(m);
            }
        }
        serde_json::to_value(out).unwrap_or(serde_json::Value::Null)
    }

    /// 按内容哈希返回文件字节（供内嵌 HTTP 服务使用）。
    pub fn get(&self, hash: &str) -> Option<Vec<u8>> {
        let path = self.map.lock().unwrap().get(hash).cloned()?;
        std::fs::read(&path).ok()
    }
}

/// 由本地路径构造 FileMeta（含 SHA-256 内容哈希）。目录仅登记元信息、不读内容。
fn build_meta(p: &PathBuf) -> Option<FileMeta> {
    let meta = std::fs::metadata(p).ok()?;
    let is_dir = meta.is_dir();
    let (file_size, hash) = if is_dir {
        (0u64, String::new())
    } else {
        let data = std::fs::read(p).ok()?;
        let digest = Sha256::digest(&data);
        (data.len() as u64, format!("{digest:x}"))
    };
    let file_name = p.file_name()?.to_string_lossy().to_string();
    Some(FileMeta {
        file_name,
        file_size,
        is_dir,
        relative_path: String::new(),
        modified_at: 0,
        mime_type: String::new(),
        hash: if hash.is_empty() { None } else { Some(hash) },
    })
}
