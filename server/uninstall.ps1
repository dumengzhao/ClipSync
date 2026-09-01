# 以管理员身份运行：卸载 clipsync-server 服务
$ErrorActionPreference = "Stop"

$serviceName = "ClipSyncServer"

$svc = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if ($svc) {
    try { sc.exe stop $serviceName | Out-Null } catch {}
    Start-Sleep -Seconds 1
    sc.exe delete $serviceName | Out-Null
    Write-Host "已删除服务 $serviceName"
} else {
    Write-Host "服务不存在，无需卸载"
}

# 清理防火墙规则
Remove-NetFirewallRule -DisplayName "ClipSync Relay Server (20070)" -ErrorAction SilentlyContinue
Write-Host "已清理防火墙规则"
