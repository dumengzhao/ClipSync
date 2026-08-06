//! 文件缓存管理 - LRU 淘汰 + TTL
//!
//! 用于已传输文件的本地缓存，避免重复传输（秒传）。MVP 提供内存 LRU + TTL，
//! 后续阶段接入磁盘持久化与自动清理。

use crate::clipboard::types::SyncId;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub path: PathBuf,
    pub size: u64,
    pub stored_at: Instant,
}

impl CacheEntry {
    pub fn new(path: PathBuf, size: u64) -> Self {
        Self {
            path,
            size,
            stored_at: Instant::now(),
        }
    }
}

pub struct FileCache {
    inner: LruCache<String, CacheEntry>,
    ttl: Duration,
}

impl FileCache {
    pub fn new(capacity: usize, ttl_hours: u32) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).unwrap();
        Self {
            inner: LruCache::new(cap),
            ttl: Duration::from_secs((ttl_hours as u64).max(1) * 3600),
        }
    }

    pub fn insert(&mut self, key: &str, entry: CacheEntry) {
        self.inner.put(key.to_string(), entry);
    }

    /// 返回未过期的缓存路径；已过期视为未命中
    pub fn get(&mut self, key: &str) -> Option<PathBuf> {
        let entry = self.inner.get(key)?;
        if entry.stored_at.elapsed() > self.ttl {
            return None;
        }
        Some(entry.path.clone())
    }

    pub fn contains(&mut self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// 淘汰所有过期的缓存项
    pub fn evict_expired(&mut self) {
        let expired: Vec<String> = self
            .inner
            .iter()
            .filter(|(_, e)| e.stored_at.elapsed() > self.ttl)
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired {
            self.inner.pop(&k);
        }
    }
}

impl Default for FileCache {
    fn default() -> Self {
        Self::new(256, 24)
    }
}

/// 以 sync_id + file_index 构造缓存键（秒传去重用）
pub fn cache_key_for(sync_id: &SyncId, file_index: usize) -> String {
    format!("{}-{}", sync_id.0, file_index)
}
