#!/usr/bin/env bash
#
# ClipSync Relay Server 一键部署（在 Linux 主机上以 root 运行）
# 自动完成：建用户/目录 -> 装二进制 + service + env -> 随机密码 -> 启用并启动 systemd。
#
# 用法：
#   sudo ./install.sh                         # 自动找脚本同目录 / target 下的 clipsync-server
#   sudo ./install.sh /path/to/clipsync-server # 显式指定二进制
#   sudo ./install.sh --no-start               # 只安装不启动
set -euo pipefail

BIN_ARG=""
DO_START=1
while [ $# -gt 0 ]; do
  case "$1" in
    --no-start) DO_START=0 ;;
    --*) echo "未知参数: $1" >&2; exit 1 ;;
    *) BIN_ARG="$1" ;;
  esac
  shift
done

INSTALL_DIR=/opt/clipsync-server
RUN_USER=clipsync
RUN_GROUP=clipsync
SVC=clipsync-server
PORT=20070

[ "$(id -u)" -eq 0 ] || { echo "请使用 root 运行：sudo $0" >&2; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
echo ">>> 脚本所在目录(源路径): $SCRIPT_DIR"

# 定位二进制
find_binary() {
  local cands=()
  [ -n "$BIN_ARG" ] && cands+=("$BIN_ARG")
  cands+=("$SCRIPT_DIR/clipsync-server"
          "$SCRIPT_DIR/target/x86_64-unknown-linux-musl/release/clipsync-server"
          "$SCRIPT_DIR/target/release/clipsync-server")
  for c in "${cands[@]}"; do
    [ -n "$c" ] && [ -f "$c" ] && { echo "$c"; return 0; }
  done
  return 1
}

BIN="$(find_binary)" || {
  echo "找不到 clipsync-server 二进制。" >&2
  echo "请把二进制放到脚本同目录，或：sudo $0 /path/to/clipsync-server" >&2
  exit 1
}
echo ">>> 使用二进制: $BIN"

# 更新检测：若服务已在运行，先停掉以释放旧二进制（避免覆盖正在执行的文件）
WAS_ACTIVE=0
if systemctl is-active --quiet "$SVC"; then WAS_ACTIVE=1; fi
if [ "$WAS_ACTIVE" -eq 1 ]; then
  systemctl stop "$SVC"
  echo ">>> 检测到已运行服务，已停止（准备更新）"
fi

# 建用户/目录
if ! id "$RUN_USER" &>/dev/null; then
  useradd -r -d "$INSTALL_DIR" -s /usr/sbin/nologin "$RUN_USER"
  echo ">>> 已创建用户 $RUN_USER"
fi
mkdir -p "$INSTALL_DIR/data"
install -m 0755 "$BIN" "$INSTALL_DIR/clipsync-server"
echo ">>> 已安装二进制 -> $INSTALL_DIR/clipsync-server"

# service 单元：优先用脚本同目录的 clipsync-server.service，否则用内嵌模板
install_service() {
  if [ -f "$SCRIPT_DIR/clipsync-server.service" ]; then
    install -m 0644 "$SCRIPT_DIR/clipsync-server.service" /etc/systemd/system/clipsync-server.service
    echo ">>> 已安装 service（来自同目录文件）-> /etc/systemd/system/clipsync-server.service"
    return
  fi
  cat > /etc/systemd/system/clipsync-server.service <<'SERVICE'
[Unit]
Description=ClipSync Relay Server
Documentation=https://github.com/dumengzhao/ClipSync
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=clipsync
Group=clipsync
WorkingDirectory=/opt/clipsync-server
EnvironmentFile=/opt/clipsync-server/clipsync-server.env
ExecStart=/opt/clipsync-server/clipsync-server
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
SERVICE
  echo ">>> 已安装 service（来自内嵌模板）-> /etc/systemd/system/clipsync-server.service"
}
install_service

# env：首次部署用示例生成；**已存在则原样保留**（更新时线上配置不受影响）。
#
# 注意：服务端**没有任何凭据类环境变量** —— 管理员口令是 <数据目录>/admin.json 里的
# Argon2id 哈希，首次部署后在管理页「初始化」卡片粘贴自生成的哈希串（见 README）。
ENV_FILE="$INSTALL_DIR/clipsync-server.env"
if [ ! -f "$ENV_FILE" ]; then
  if [ -f "$SCRIPT_DIR/clipsync-server.env.example" ]; then
    cp "$SCRIPT_DIR/clipsync-server.env.example" "$ENV_FILE"
  else
    {
      printf 'CLIPSYNC_DATA_DIR=%s\n' "$INSTALL_DIR/data"
      printf 'LISTEN=0.0.0.0:%s\n' "$PORT"
    } > "$ENV_FILE"
  fi
  echo ">>> 已生成 env -> $ENV_FILE (权限 600)"
fi
chmod 600 "$ENV_FILE"

chown -R "$RUN_USER:$RUN_GROUP" "$INSTALL_DIR"
echo ">>> 目录权限已设给 $RUN_USER:$RUN_GROUP"

if [ "$DO_START" -eq 1 ]; then
  systemctl daemon-reload
  if [ "$WAS_ACTIVE" -eq 1 ]; then
    systemctl restart "$SVC"
    echo ">>> 更新完成：服务已用新二进制重启"
  else
    systemctl enable --now "$SVC"
    echo ">>> 部署完成：服务已启用并启动"
  fi
  sleep 1
  if systemctl is-active --quiet "$SVC"; then
    echo ">>> 服务状态: $(systemctl is-active "$SVC")"
    echo ">>> 健康检查: $(curl -fsS "http://127.0.0.1:$PORT/healthz" || echo '无响应(服务可能还在起)')"
  else
    echo ">>> 服务未运行。业务日志只写文件：tail -n 50 $INSTALL_DIR/data/logs/clipsync-server.log.*" >&2
    echo ">>> （启动失败的 panic 才在 journal 里：journalctl -u $SVC -n 50）" >&2
    exit 1
  fi
fi

echo
echo "部署完成。可选下一步：配置 nginx 反代（见 nginx-clipsync.conf.example），"
echo "走 nginx TLS 时桌面端服务器地址填 wss://域名。"
echo
echo "若还没初始化过管理员口令（本机没有 admin.json），还需要："
echo "  1) $INSTALL_DIR/clipsync-server hash          # 交互输入口令，输出 Argon2id 哈希串"
echo "  2) 打开 http://<服务器地址>:$PORT/admin ，在「初始化」卡片里粘贴哈希串 + 用户名"
echo "  （服务端不存明文口令；忘记口令就删 $INSTALL_DIR/data/admin.json 重启重来）"
