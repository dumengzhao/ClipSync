Unicode true
; ClipSync Relay Server 安装包（NSIS）
; 编译：在 server/ 目录下执行 makensis installer.nsi
; 需要管理员权限（注册 Windows 服务 + 放行防火墙）
!include "MUI2.nsh"
!include "nsDialogs.nsh"
!include "LogicLib.nsh"

Name "ClipSync Relay Server"
OutFile "clipsync-server-setup.exe"
InstallDir "C:\ClipSyncServer"
RequestExecutionLevel admin
SetCompressor lzma

!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_DIRECTORY
Page custom nsdPasswordPage nsdPasswordPageLeave
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"

Var AdminPassword

Function nsdPasswordPage
    nsDialogs::Create 1018
    Pop $R0
    ${If} $R0 == error
        Abort
    ${EndIf}
    ${NSD_CreateLabel} 0 0 100% 12u "设置管理后台密码（ADMIN_PASS，用于 http://localhost:20070/admin 登录）："
    ${NSD_CreatePassword} 0 16u 100% 12u ""
    Pop $AdminPassword
    nsDialogs::Show
FunctionEnd

Function nsdPasswordPageLeave
    ${NSD_GetText} $AdminPassword $AdminPassword
    ${If} $AdminPassword == ""
        MessageBox MB_OK|MB_ICONEXCLAMATION "请输入管理后台密码"
        Abort
    ${EndIf}
FunctionEnd

Section "Install"
    SetOutPath $INSTDIR
    File "target\release\clipsync-server.exe"
    WriteUninstaller "$INSTDIR\uninstall.exe"

    ; 数据目录（ProgramData）
    ReadEnvStr $0 "ProgramData"
    StrCpy $0 "$0\ClipSyncServer\data"
    CreateDirectory "$0"

    ; 通过注册表给服务进程注入环境变量（服务不继承安装程序环境，必须是 REG_MULTI_SZ）
    ExecWait 'reg add "HKLM\SYSTEM\CurrentControlSet\Services\ClipSyncServer" /v Environment /t REG_MULTI_SZ /d "CLIPSYNC_DATA_DIR=$0\0ADMIN_USER=admin\0ADMIN_PASS=$AdminPassword\0LISTEN=0.0.0.0:20070" /f'

    ; 注册为自启 Windows 服务（binPath 带 --service 走 SCM 模式）
    ExecWait 'sc.exe create ClipSyncServer binPath= "$INSTDIR\clipsync-server.exe --service" start= auto DisplayName= "ClipSync Relay Server"'
    ExecWait 'sc.exe failure ClipSyncServer reset= 0 actions= restart/60000'
    ; 放行防火墙入站 20070
    ExecWait 'netsh advfirewall firewall add rule name="ClipSync Relay Server (20070)" dir=in action=allow protocol=TCP localport=20070'
    ; 启动
    ExecWait 'sc.exe start ClipSyncServer'

    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\ClipSyncServer" "DisplayName" "ClipSync Relay Server"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\ClipSyncServer" "UninstallString" "$INSTDIR\uninstall.exe"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\ClipSyncServer" "InstallLocation" $INSTDIR
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\ClipSyncServer" "Publisher" "ClipSync"
SectionEnd

Section "Uninstall"
    ExecWait 'sc.exe stop ClipSyncServer'
    ExecWait 'sc.exe delete ClipSyncServer'
    ExecWait 'netsh advfirewall firewall delete rule name="ClipSync Relay Server (20070)"'
    RMDir /r "$INSTDIR"
    ReadEnvStr $1 "ProgramData"
    RMDir /r "$1\ClipSyncServer"
    DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\ClipSyncServer"
SectionEnd
