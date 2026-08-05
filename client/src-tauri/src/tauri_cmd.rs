//! Tauri 命令定义
//!
//! 前端通过 `invoke()` 调用这些命令

use crate::clipboard::types::DeviceId;

#[tauri::command]
pub fn get_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[tauri::command]
pub fn get_device_id() -> String {
    // TODO: 阶段一实现 - 从配置加载或生成新的设备 ID
    DeviceId("placeholder".to_string()).0
}

#[tauri::command]
pub fn get_paired_devices() -> Vec<String> {
    // TODO: 阶段二实现 - 返回已配对设备列表
    Vec::new()
}
