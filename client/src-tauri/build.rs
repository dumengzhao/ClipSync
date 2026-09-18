fn main() {
    // Windows：**测试目标**需要显式声明 ComCtl32 v6 依赖。
    //
    // 背景（2026-09-18 查明）：`rfd`（tauri-plugin-dialog 的依赖）在 Windows 上静态导入
    // `comctl32.dll!TaskDialogIndirect`，该导出**只有 ComCtl32 v6** 提供；而 cargo 链接
    // 测试目标时默认**不嵌入**带 v6 依赖的清单 → 进程加载期就报
    // `0xc0000139 STATUS_ENTRYPOINT_NOT_FOUND`（表现为 `cargo test` 全部失败，
    // 本机与 windows CI 都是这个原因，与用例本身无关）。
    // 正式 exe 不受影响：tauri 自己的资源清单已含该依赖。
    //
    // 用 `rustc-link-arg`（对本 crate 的可执行目标与测试目标都生效；rlib 不参与链接，
    // 所以库本身不受影响）。这里用 **MANIFESTDEPENDENCY**（往清单里追加一条依赖）
    // 而不是 MANIFESTINPUT（会再嵌一份 MANIFEST 资源）——后者与 bins 里已有的清单
    // 资源冲突（CVT1100: 资源重复），前者只是追加依赖，安全。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!(
            "cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
    tauri_build::build()
}
