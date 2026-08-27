// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // GNOME/Wayland 下 Tauri/GTK 的窗口拖动（startDragging）会被合成器忽略，
    // 强制走 X11 会话（Xwayland）以恢复原生窗口拖动。仅 Linux 生效，win/mac 忽略。
    #[cfg(target_os = "linux")]
    std::env::set_var("GDK_BACKEND", "x11");

    clipsync_lib::run()
}
