//! 跨平台硬件唯一标识（机器码）
//!
//! 单一来源：身份模块（device/identity）与服务端上报（server_conn）共用。
//! 优先级：macOS IOPlatformUUID / Windows MachineGuid / Linux /etc/machine-id；
//! 均失败时返回空串，由调用方自行兜底（如生成 fallback 身份）。

/// 跨平台硬件唯一标识。
/// Windows 下 reg 是控制台程序：已设 CREATE_NO_WINDOW，不会闪出命令窗口。
pub fn hardware_id() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sh")
            .arg("-c")
            .arg("ioreg -rd1 -c IOPlatformExpertDevice 2>/dev/null | grep IOPlatformUUID")
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout);
            // 形如：  "IOPlatformUUID" = "XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX"
            let parts: Vec<&str> = s.split('"').collect();
            if parts.len() >= 4 {
                let h = parts[3].trim().to_string();
                if !h.is_empty() {
                    return h;
                }
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        // reg 是控制台程序：不设 CREATE_NO_WINDOW 会闪出命令窗口
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if let Ok(out) = std::process::Command::new("reg")
            .args([
                "query",
                "HKLM\\SOFTWARE\\Microsoft\\Cryptography",
                "/v",
                "MachineGuid",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(h) = s.split_whitespace().last() {
                let h = h.trim().to_string();
                if !h.is_empty() {
                    return h;
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/etc/machine-id") {
            let s = s.trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    String::new()
}
