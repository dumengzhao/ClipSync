//! 落盘名字净化 —— 所有来自对端（不受信）的名字都必须经过这里。
//!
//! ## 设计约定（用户确认）
//!
//! 接收方**不还原对端的目录层级**：`FileMeta.relative_path` 一律不使用，
//! 所有文件平铺到 `sync_dir/<设备名>/` 下，同名直接覆盖（这是正常行为）。
//! 落盘路径因此只由两个受净化的单段名字拼成：
//!
//! ```text
//! sync_dir / safe_segment(device_name) / safe_segment(file_name)
//! ```
//!
//! 这既贴合需求，也从根上消除了两类漏洞：
//! - **相对路径穿越**：`../../..` 被 `Path::file_name()` 剥成最后一段；
//! - **绝对路径替换**：`C:\Windows\x.bat`、`/etc/passwd` 同样只留文件名，
//!   不会让 `PathBuf::join` 丢弃前缀（join 遇绝对路径会整体替换，实测确认过）。

use std::path::Path;
use tauri::Manager;

/// 单段名字的最大字符数（超出截断；Windows 单段上限 255，这里取更保守的值）
const MAX_SEGMENT_CHARS: usize = 120;

/// 保留设备名（Windows 上即便带扩展名也不可作文件名），命中则前缀下划线。
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 取路径的最后一段：同时处理 `/` 与 `\`，丢弃盘符与所有目录部分。
///
/// 不能直接依赖 `Path::file_name()` 的平台语义——它只识别**本平台**的分隔符
/// （Linux 上不认 `\`，会把 `C:\Windows\x.bat` 整串当文件名）。对端可能发来
/// Windows 风格路径，因此先把 `\` 归一成 `/` 再取最后一段，三平台行为一致。
fn last_segment(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.replace('\\', "/");
    if let Some(name) = Path::new(&normalized).file_name() {
        return Some(name.to_string_lossy().into_owned());
    }
    // 退化路径（例如整串就是 `/` 或只剩分隔符）：再手工取一次最后一段
    let tail = normalized.rsplit('/').next().unwrap_or("");
    if tail.is_empty() || tail == "." || tail == ".." {
        None
    } else {
        Some(tail.to_string())
    }
}

/// 把不受信名字净化成**单段**安全名字（设备名与文件名共用）。
///
/// 规则：只取最后一段 → 去控制字符与 Windows 非法字符（替换为 `_`）→
/// 拒绝空/`.`/`..` → 规避 Windows 保留名 → 限长。永不返回带分隔符的结果。
pub fn safe_segment(raw: &str) -> String {
    let Some(seg) = last_segment(raw) else {
        return "unnamed".to_string();
    };
    let mut cleaned: String = seg
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            // Windows 不允许出现在文件名中的字符
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect();
    cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return "unnamed".to_string();
    }
    if cleaned.chars().count() > MAX_SEGMENT_CHARS {
        cleaned = cleaned.chars().take(MAX_SEGMENT_CHARS).collect();
    }
    // 保留名：追加下划线前缀（同时覆盖「带扩展名」的写法，如 CON.txt）
    let stem_upper = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED_NAMES.contains(&stem_upper.as_str()) {
        cleaned = format!("_{cleaned}");
    }
    cleaned
}

/// 解析落盘根目录 —— **P2P 与跨 LAN 两条传输路径共用同一套规则**。
///
/// 优先级：显式配置的 `sync_dir`（去空白后非空）→ 系统「下载」目录 → 相对路径 `"Downloads"`。
///
/// 之所以提取到公共位置：早先跨 LAN 路径**单独回退到 `temp_dir()/clipsync`**，而配置项
/// `sync_dir` 只在 P2P 路径生效，结果是**同一台设备按传输路径不同把文件落到两个地方**
/// （下载目录 vs 临时目录），既不一致也难排查。
pub fn resolve_sync_dir(
    app: Option<&tauri::AppHandle>,
    configured: Option<String>,
) -> std::path::PathBuf {
    if let Some(dir) = configured {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return std::path::PathBuf::from(trimmed);
        }
    }
    if let Some(a) = app {
        if let Ok(dir) = a.path().download_dir() {
            return dir;
        }
    }
    std::path::PathBuf::from("Downloads")
}

#[cfg(test)]
mod tests {
    use super::safe_segment;
    #[test]
    fn strips_traversal_and_absolute_paths() {
        assert_eq!(safe_segment("../../../../x.txt"), "x.txt");
        assert_eq!(safe_segment(r"C:\Windows\System32\evil.bat"), "evil.bat");
        assert_eq!(safe_segment("/etc/passwd"), "passwd");
        assert_eq!(safe_segment(r"..\..\a\b\c.txt"), "c.txt");
    }

    #[test]
    fn rejects_dot_only_and_empty() {
        assert_eq!(safe_segment(".."), "unnamed");
        assert_eq!(safe_segment("."), "unnamed");
        assert_eq!(safe_segment(""), "unnamed");
        assert_eq!(safe_segment("   "), "unnamed");
        assert_eq!(safe_segment("/"), "unnamed");
    }

    #[test]
    fn keeps_ordinary_names_including_chinese() {
        assert_eq!(safe_segment("photo.png"), "photo.png");
        assert_eq!(safe_segment("我的文档.txt"), "我的文档.txt");
        assert_eq!(safe_segment("nested/sub/doc.txt"), "doc.txt");
    }

    #[test]
    fn handles_windows_reserved_names() {
        assert_eq!(safe_segment("CON"), "_CON");
        assert_eq!(safe_segment("nul.txt"), "_nul.txt");
        assert_eq!(safe_segment("normal"), "normal");
    }

    #[test]
    fn truncates_overlong_names() {
        let long = "a".repeat(500);
        assert_eq!(safe_segment(&long).chars().count(), 120);
    }
}
