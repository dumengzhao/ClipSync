//! 配置 schema 迁移
//!
//! 校验加载的配置版本，必要时迁移到当前 schema。当前版本（v1）无破坏性变更，
//! 提供 no-op 迁移入口与版本守卫，便于后续版本升级时扩展。

use serde::{Deserialize, Serialize};

pub const CURRENT_CONFIG_VERSION: u32 = 1;

/// 落盘配置文件外壳，携带 schema 版本号
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    pub version: u32,
    #[serde(flatten)]
    pub config: crate::config::AppConfig,
}

/// 版本是否兼容当前程序。未来出现不兼容 schema 时返回 false。
pub fn is_compatible(version: u32) -> bool {
    version <= CURRENT_CONFIG_VERSION
}

/// 将旧版本配置迁移到当前 schema（当前为 no-op，仅断言兼容并规范化版本号）。
pub fn migrate(mut cfg: ConfigFile) -> anyhow::Result<ConfigFile> {
    if cfg.version > CURRENT_CONFIG_VERSION {
        anyhow::bail!(
            "config version {} is newer than supported {}",
            cfg.version,
            CURRENT_CONFIG_VERSION
        );
    }
    cfg.version = CURRENT_CONFIG_VERSION;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_bumps_old_version() {
        let cfg = ConfigFile {
            version: 0,
            config: crate::config::AppConfig::default(),
        };
        let out = migrate(cfg).unwrap();
        assert_eq!(out.version, CURRENT_CONFIG_VERSION);
    }

    #[test]
    fn migrate_keeps_current_version() {
        let cfg = ConfigFile {
            version: CURRENT_CONFIG_VERSION,
            config: crate::config::AppConfig::default(),
        };
        let out = migrate(cfg).unwrap();
        assert_eq!(out.version, CURRENT_CONFIG_VERSION);
    }

    #[test]
    fn migrate_rejects_future_version() {
        let cfg = ConfigFile {
            version: 999,
            config: crate::config::AppConfig::default(),
        };
        assert!(migrate(cfg).is_err());
    }

    #[test]
    fn compatible_check() {
        assert!(is_compatible(0));
        assert!(is_compatible(CURRENT_CONFIG_VERSION));
        assert!(!is_compatible(CURRENT_CONFIG_VERSION + 1));
    }
}
