#!/usr/bin/env bash
#
# ClipSync Relay Server 一键停止（暂停）（在 Linux 主机上以 root 运行）
# 仅停止已安装的服务，保留二进制/配置/env。常用于维护或排障时暂停。
#
# 用法：
#   sudo ./stop.sh
set -euo pipefail

SVC=clipsync-server

[ "$(id -u)" -eq 0 ] || { echo "请使用 root 运行：sudo $0" >&2; exit 1; }

if ! systemctl list-unit-files "$SVC.service" &>/dev/null; then
  echo "未找到 $SVC 服务，请先运行 install.sh 部署。" >&2
  exit 1
fi

if systemctl is-active --quiet "$SVC"; then
  systemctl stop "$SVC"
  echo ">>> 已停止 $SVC"
else
  echo ">>> $SVC 当前未运行，无需停止"
fi
