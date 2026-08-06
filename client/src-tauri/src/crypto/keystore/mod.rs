//! 平台密钥链封装
//!
//! 与 `clipboard/` 模块同构：按平台拆分独立文件，通过 `#[cfg(target_os)]`
//! 条件编译，由本文件统一导出 `store` / `load` 两个无状态函数。
//!
//! - `windows.rs`：Windows Credential Manager (Win32 CredWrite/CredRead)
//! - `macos.rs`：macOS Keychain (security-framework)
//! - `linux.rs`：Linux Secret Service (dbus-secret-service)
//!
//! 设计说明：仅提供 `store` / `load` 两个无状态函数，不引入 trait
//! （区别于 clipboard 的 `ClipboardProvider`），避免过度抽象。

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::{delete, load, store};
#[cfg(target_os = "macos")]
pub use macos::{delete, load, store};
#[cfg(target_os = "windows")]
pub use windows::{delete, load, store};
