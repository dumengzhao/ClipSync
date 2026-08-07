#!/usr/bin/env bash
# 重启 ClipSync 开发服务器：先停止现有进程，再后台启动 tauri dev
set -uo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
CLIENT="$ROOT/client"

# 复用停止脚本，确保端口释放、无残留进程
"$ROOT/scripts/stop.sh"

# 加载 cargo 环境（rustup 安装时不会自动写入非交互 shell）
if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck source=/dev/null
    . "$HOME/.cargo/env"
fi

# Linux 下禁用 dmabuf / 强制软件合成，规避部分 Wayland/虚拟机环境的
# EGL/ZINK 渲染警告导致 webview 初始化失败的问题。其它平台忽略这些变量。
export WEBKIT_DISABLE_DMABUF_RENDERER=1
export WEBKIT_DISABLE_COMPOSITING_MODE=1

echo "启动 ClipSync (tauri dev)..."
cd "$CLIENT"
exec npm run tauri dev
