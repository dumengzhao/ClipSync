//! 服务端日志初始化。
//!
//! 两路输出，同一份内容：
//! - **stdout** → 交给 systemd（`journalctl -u clipsync-server` 看，也方便前台直接跑）；
//! - **文件** → `<日志目录>/clipsync-server.log.YYYY-MM-DD`，**按天滚动**。这是为了让人
//!   不用登上去敲 journalctl 也能排查（尤其是「定时拉取到底跑没跑」这类问题）。
//!
//! 时间戳用**本机时区**（cron 表达式也是按本地时区解释的，两边口径一致，不然看日志对不上）。
//!
//! 级别：`CLIPSYNC_LOG_LEVEL`（默认 `info`），也兼容 `RUST_LOG` 的第一个词
//! （不用 env-filter，省一组依赖）。字符数少，写死几档就够。

use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry;

/// 日志文件名前缀；实际文件是 `<前缀>.YYYY-MM-DD`。
const LOG_FILE_PREFIX: &str = "clipsync-server.log";
/// 保留天数（`tracing_appender` 自己不做清理，我们启动时与运行期定期扫）。
const LOG_KEEP_DAYS: u64 = 14;
/// 运行期清理间隔：服务可能长跑几个月，只在启动时清一次的话日志会越堆越多。
pub const PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// 把**外部可控**的字符串收拾成适合写日志的样子：控制字符换成空格、超长截断。
///
/// 为什么要专门做这件事：日志是给人看的诊断依据，里面混进一个换行就能**伪造日志行** ——
/// 比如设备 id 传成 `"abc\n2026-01-01T00:00:00+08:00 INFO 同步完成"`，事后排查就会被带偏
/// （看起来像真的发生过一次同步）。设备 id / 仓库版本号这类值都来自外部，进日志前一律过这里。
///
/// 单条上限 200 字符：日志是诊断用的，不需要把整段输入搬进去。
pub fn clean(s: &str) -> String {
    const MAX_CHARS: usize = 200;
    let mut out = String::with_capacity(s.len().min(MAX_CHARS * 4));
    for (i, c) in s.chars().enumerate() {
        if i >= MAX_CHARS {
            out.push('…');
            break;
        }
        out.push(if c.is_control() { ' ' } else { c });
    }
    out
}

/// 本机时区的时间戳：`2026-09-26T17:03:31.123+08:00`。
struct LocalTimer;

impl FormatTime for LocalTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(
            w,
            "{}",
            chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z")
        )
    }
}

/// 解析日志级别字符串。`RUST_LOG` 常见写法是 `crate=debug,other=info`，
/// 取第一个词、再截掉 `xxx=` 部分。
fn parse_level(raw: &str) -> tracing::Level {
    let first = raw
        .split(',')
        .next()
        .unwrap_or("")
        .rsplit('=')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match first.as_str() {
        "trace" => tracing::Level::TRACE,
        "debug" => tracing::Level::DEBUG,
        "warn" | "warning" => tracing::Level::WARN,
        "error" => tracing::Level::ERROR,
        _ => tracing::Level::INFO,
    }
}

/// 读取环境变量决定级别：`CLIPSYNC_LOG_LEVEL` 优先，其次 `RUST_LOG`，最后默认 `info`。
fn log_level() -> tracing::Level {
    let raw = std::env::var("CLIPSYNC_LOG_LEVEL")
        .ok()
        .or_else(|| std::env::var("RUST_LOG").ok())
        .unwrap_or_default();
    parse_level(&raw)
}

/// 日志目录：`CLIPSYNC_LOG_DIR` 优先，否则 `<data_dir>/logs`。
pub fn log_dir(data_dir: &str) -> std::path::PathBuf {
    std::env::var("CLIPSYNC_LOG_DIR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(data_dir).join("logs"))
}

/// 清理过期日志（启动时调用一次，之后由 `PRUNE_INTERVAL` 周期调用）。
pub fn prune(data_dir: &str) {
    prune_old_logs(&log_dir(data_dir));
}

/// 清理超过保留期的旧日志（`clipsync-server.log.YYYY-MM-DD`）。
///
/// 失败只记一条 warn —— 清不掉旧日志不该影响服务启动。
fn prune_old_logs(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(LOG_KEEP_DAYS * 86_400));
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(LOG_FILE_PREFIX) {
            continue;
        }
        let Ok(md) = entry.metadata() else { continue };
        let Ok(mtime) = md.modified() else { continue };
        if let Some(cutoff) = cutoff {
            if mtime < cutoff {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// 初始化日志。幂等：重复调用只会保留第一次的设置（`try_init` 失败即忽略）。
pub fn init(data_dir: &str) {
    let level = log_level();
    let dir = log_dir(data_dir);
    let mut file_layer = None;
    match std::fs::create_dir_all(&dir) {
        Ok(()) => {
            // 日志里有设备 id、来源 IP、时间线，不该是本机所有用户都能读的
            // （只对 Unix 设：Windows 上 mode 只映射成只读位，反而会挡住后续写入）
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
            }
            prune_old_logs(&dir);
            let appender = tracing_appender::rolling::daily(&dir, LOG_FILE_PREFIX);
            file_layer = Some(
                tracing_subscriber::fmt::layer()
                    .with_timer(LocalTimer)
                    .with_ansi(false) // 文件里不要颜色转义
                    .with_target(false)
                    .with_writer(appender),
            );
        }
        Err(e) => {
            // 日志目录建不出来就只走 stdout，别让服务起不来
            eprintln!(
                "[clipsync-server] 无法创建日志目录 {}：{e}（只输出到 stdout）",
                dir.display()
            );
        }
    }
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_timer(LocalTimer)
        .with_target(false)
        .with_writer(std::io::stdout);

    if let Err(e) = registry()
        .with(stdout_layer)
        .with(file_layer)
        .with(tracing_subscriber::filter::LevelFilter::from_level(level))
        .try_init()
    {
        // 只可能是「已经有全局 subscriber」（我们只在启动时调一次）；真到这一步说明
        // 日志没接上，用 eprintln 兜一句，免得后面出了问题什么线索都没有。
        eprintln!("[clipsync-server] 日志未初始化成功：{e}");
    }

    tracing::info!(
        "日志已启动：级别={level}，日志目录={}（按天滚动，保留 {LOG_KEEP_DAYS} 天）",
        dir.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_control_chars_and_caps_length() {
        // 换行/回车/制表/ESC 都会被换成空格 —— 否则可以伪造出一行假日志
        let forged = "dev-1\n2026-01-01T00:00:00+08:00 INFO 同步完成";
        let out = clean(forged);
        assert!(!out.contains('\n'), "换行必须被清掉: {out}");
        assert!(out.starts_with("dev-1 "));

        assert_eq!(clean("normal-device"), "normal-device");
        assert_eq!(clean("tab\there"), "tab here");

        // 超长截断，且不 panic（按字符而非字节）
        let long = "设".repeat(500);
        let out = clean(&long);
        assert_eq!(out.chars().count(), 201, "200 字符 + 省略号");
        assert!(out.ends_with('…'));
    }

    #[test]
    fn log_level_defaults_to_info_and_reads_env_forms() {
        assert_eq!(parse_level(""), tracing::Level::INFO);
        assert_eq!(parse_level("debug"), tracing::Level::DEBUG);
        assert_eq!(parse_level("WARN"), tracing::Level::WARN);
        // RUST_LOG 的常见写法：`crate=level,other=level`
        assert_eq!(parse_level("clipsync_server=trace"), tracing::Level::TRACE);
        assert_eq!(parse_level("error,hyper=warn"), tracing::Level::ERROR);
        // 认不出来就当 info，不要因为写错而把日志全关掉
        assert_eq!(parse_level("乱写的"), tracing::Level::INFO);
    }
}
