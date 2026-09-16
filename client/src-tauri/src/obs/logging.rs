//! 日志轮转 - tracing + tracing-appender
//!
//! 按天滚动，保留最近 7 天；同时输出到 stderr 便于开发调试。
//! 进程启动时调用一次 `init_file_logging()`。

use std::path::PathBuf;
use tracing_appender::rolling;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// 平台日志目录
pub fn log_dir() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("clipsync").join("logs");
    }
    if let Some(home) = std::env::var("HOME").ok().filter(|s| !s.is_empty()) {
        #[cfg(target_os = "macos")]
        {
            return PathBuf::from(home)
                .join("Library")
                .join("Logs")
                .join("clipsync");
        }
        #[cfg(target_os = "linux")]
        {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("clipsync")
                .join("logs");
        }
        #[allow(unreachable_code)]
        {
            return PathBuf::from(home).join("clipsync-logs");
        }
    }
    PathBuf::from("logs")
}

/// 初始化文件 + stderr 日志。重复调用会被 tracing 忽略（已初始化）。
pub fn init_file_logging() {
    let dir = log_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("failed to create log dir {:?}: {e}", dir);
        return;
    }

    let file_appender = rolling::daily(&dir, "clipsync.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    // mdns-sd 在多网卡主机上会对每个链路本地 IPv6 地址刷 ERROR
    // （`Cannot find valid addrs for TYPE_SRV/TYPE_A ...`），属已知的无害噪音，
    // 但会把真正有用的同步日志淹没，故默认关闭其自带日志（本项目在
    // `discovery::mdns` 里有自己的记录）。需要排查 mDNS 时用
    // `RUST_LOG=info,clipsync=debug,mdns_sd=debug` 覆盖即可。
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,clipsync=debug,mdns_sd=off"));

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_target(false)
        .with_ansi(false);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    // 保持后台写入 worker 存活到进程结束（logger 全生命周期需要）
    std::mem::forget(guard);
}

/// 净化**对端可控**字符串后再写日志。
///
/// 对端自报的 `device_name` / 文件名 / 请求路径都会进日志，其中可以包含 `\r\n` 与
/// 其它控制字符（bincode 与 HTTP 头都不做字符集过滤）——不净化就等于让对端伪造、
/// 污染本机日志行（日志注入：可插入看似来自本机的记录，干扰排查与审计）。
/// 处理：控制字符（含换行/制表）统一替换为 `?`，并按字符数截断，避免超长刷屏。
pub fn log_safe(raw: &str) -> String {
    const MAX_CHARS: usize = 200;
    let mut out = String::with_capacity(raw.len().min(MAX_CHARS));
    for (i, c) in raw.chars().enumerate() {
        if i >= MAX_CHARS {
            out.push_str("…(截断)");
            break;
        }
        if c.is_control() || c == '\u{2028}' || c == '\u{2029}' {
            out.push('?');
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::log_safe;

    /// 换行/回车等控制字符必须被替换，防止对端伪造日志行。
    #[test]
    fn log_safe_strips_control_chars() {
        assert_eq!(
            log_safe("evil\r\n2026-01-01 INFO 已配对"),
            "evil??2026-01-01 INFO 已配对"
        );
        assert_eq!(log_safe("a\tb\u{0007}c"), "a?b?c");
        // 正常 UTF-8 名称不受影响
        assert_eq!(log_safe("我的电脑-Mac"), "我的电脑-Mac");
    }

    /// 超长输入截断，避免对端用超长名字刷爆日志。
    #[test]
    fn log_safe_truncates_long_input() {
        let long = "x".repeat(1000);
        let out = log_safe(&long);
        assert!(out.chars().count() < 220);
        assert!(out.ends_with("…(截断)"));
    }
}
