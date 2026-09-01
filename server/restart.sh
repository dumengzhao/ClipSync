#!/usr/bin/env bash
#
# ClipSync Relay Server 一键重启（在 Linux 主机上以 root 运行）
# 只重启已安装的服务，不替换二进制/配置/env。常用于改了 LISTEN 等配置或手动换二进制后生效。
#
# 用法：
#   sudo ./restart.sh
set -euo pipefail

SVC=clipsync-server
PORT=20070

[ "$(id -u)" -eq 0 ] || { echo "请使用 root 运行：sudo $0" >&2; exit 1; }

if ! systemctl list-unit-files "$SVC.service" &>/dev/null; then
  echo "未找到 $SVC 服务，请先运行 install.sh 部署。" >&2
  exit 1
fi

if ! systemctl is-active --quiet "$SVC"; then
  echo ">>> $SVC 当前未运行，改为启动"
  systemctl start "$SVC"
else
  systemctl restart "$SVC"
  echo ">>> 已重启 $SVC"
fi

sleep 1
if systemctl is-active --quiet "$SVC"; then
  echo ">>> 服务状态: $(systemctl is-active "$SVC")"
  echo ">>> 健康检查: $(curl -fsS "http://127.0.0.1:$PORT/healthz" || echo '无响应(服务可能还在起)')"
else
  echo ">>> 服务未运行，查看日志: journalctl -u $SVC -n 50" >&2
  exit 1
fi
