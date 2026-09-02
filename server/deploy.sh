#!/usr/bin/env bash
#
# 一键部署到腾讯云中继服务端（在拥有 root@152.136.229.188 SSH 密钥的机器上运行）。
# 前置：先 package.sh 生成 dist/clipsync-server-linux/。
# 用法：
#   ./deploy.sh                  # 默认部署到 root@152.136.229.188
#   ./deploy.sh user@host:port   # 自定义目标（覆盖 HOST 默认值）
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST="$HERE/dist/clipsync-server-linux"
HOST="${1:-root@152.136.229.188}"

[ -d "$DIST" ] || {
  echo "找不到 $DIST，请先运行 package.sh 交叉编译并打包" >&2
  exit 1
}

echo ">>> 上传 $DIST -> $HOST:/tmp/"
scp -r "$DIST" "$HOST:/tmp/"

echo ">>> 远端安装（install.sh 会自动停旧进程、装新二进制、重启 systemd）"
ssh "$HOST" "bash /tmp/clipsync-server-linux/install.sh"

echo ">>> 部署完成。健康检查："
ssh "$HOST" "curl -fsS http://127.0.0.1:20070/healthz || echo '(服务可能还在起，稍后重试)'"
