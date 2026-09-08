; ClipSync NSIS 自定义钩子（.nsh）—— 由 tauri build 通过 `bundle.windows.nsis.installerHooks` 自动嵌入安装器脚本。
;
; 用途：在安装目录写入 `installed.marker` 标记文件；卸载前删除。
; 运行时 `update::is_installed_build()` 通过检测此文件判断当前进程是「安装版」还是「免安装版」。
;
; 选用「专属标记文件」而不是「同目录有无 uninstall.exe」：
; - 绿色版编译时不带此文件 → 拷到任意目录都不会被误判
; - 安装版必须经 NSIS 安装才会生成 → 单一可信路径
; - uninstall.exe 可能被杀毒软件/用户误删，标记文件更稳定
; - 卸载时同步删除，不会留下陈旧标记
;
; 宏名必须与 Tauri 约定一致（见 @tauri-apps/cli/config.schema.json installerHooks 说明）：
;   NSIS_HOOK_PREINSTALL / NSIS_HOOK_POSTINSTALL / NSIS_HOOK_PREUNINSTALL / NSIS_HOOK_POSTUNINSTALL

!macro NSIS_HOOK_POSTINSTALL
    ; 在安装目录创建标记文件（安装流程已把 exe 复制到位，$INSTDIR 已确定）。
    ; FileOpen w = 写入模式、覆盖。即使失败也不中断安装（标记缺失仅影响更新入口可见性）。
    FileOpen $0 "$INSTDIR\installed.marker" w
    FileWrite $0 "ClipSync installed build$\r$\n"
    FileClose $0
    ; 设为隐藏：普通用户在资源管理器看不到，但运行时 File::exists 能读到
    SetFileAttributes "$INSTDIR\installed.marker" HIDDEN
!macroend

!macro NSIS_HOOK_PREUNINSTALL
    ; 卸载前先删掉标记，避免已卸载目录残留导致后续误判为安装版
    Delete "$INSTDIR\installed.marker"
!macroend