# 以管理员身份运行：将 clipsync-server 注册为 Windows 服务（开机自启、后台运行）
# 用法：右键 PowerShell -> 以管理员身份运行，cd 到 server 目录后 .\install.ps1
$ErrorActionPreference = "Stop"

$serviceName = "ClipSyncServer"
$exePath = Resolve-Path ".\target\release\clipsync-server.exe"
$dataDir = "C:\ProgramData\ClipSyncServer\data"
$adminUser = "admin"
$adminPass = Read-Host -Prompt "请输入管理后台密码（ADMIN_PASS）"

# 数据目录
New-Item -ItemType Directory -Force -Path $dataDir | Out-Null

# 若已存在先删
$existing = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if ($existing) {
    sc.exe delete $serviceName | Out-Null
    Start-Sleep -Seconds 1
}

# 创建服务（binPath 必须带 --service 让程序走 SCM 模式）
sc.exe create $serviceName binPath= "`"$exePath`" --service" start= auto DisplayName= "ClipSync Relay Server" | Out-Null
if ($LASTEXITCODE -ne 0) { throw "sc create failed" }

# 失败自动重启
sc.exe failure $serviceName reset= 0 actions= restart/60000 | Out-Null

# 通过注册表给服务进程注入环境变量（服务不会继承 PowerShell 的环境）
$regPath = "HKLM:\SYSTEM\CurrentControlSet\Services\$serviceName"
$envMulti = @(
    "CLIPSYNC_DATA_DIR=$dataDir",
    "ADMIN_USER=$adminUser",
    "ADMIN_PASS=$adminPass",
    "LISTEN=0.0.0.0:20070"
) -join "`0"
Set-ItemProperty -Path $regPath -Name "Environment" -Value $envMulti -Type MultiString

# 放行防火墙入站 20070
New-NetFirewallRule -DisplayName "ClipSync Relay Server (20070)" -Direction Inbound -Action Allow -Protocol TCP -LocalPort 20070 -ErrorAction SilentlyContinue

# 启动
sc.exe start $serviceName | Out-Null
Start-Sleep -Seconds 2

$svc = Get-Service -Name $serviceName
Write-Host "服务状态: $($svc.Status)"
Write-Host "管理后台: http://localhost:20070/admin"
