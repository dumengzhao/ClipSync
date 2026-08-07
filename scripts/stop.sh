#!/usr/bin/env bash
# 停止 ClipSync 开发进程（tauri dev / vite / clipsync）并释放端口
set -uo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

echo "停止 ClipSync 开发进程..."

# 杀掉 tauri dev / vite / clipsync 进程树（按完整命令行匹配）。
# pkill 无匹配时返回 1，用 `|| true` 吞掉，避免在 set -e 下退出。
pkill -f "target/debug/clipsync" 2>/dev/null || true
pkill -f "node_modules/.bin/tauri" 2>/dev/null || true
pkill -f "node_modules/.bin/vite" 2>/dev/null || true
pkill -f "npm run tauri dev" 2>/dev/null || true

# 等待端口释放；若仍被占用则按端口强杀
for port in 1420 24681; do
    for _ in $(seq 1 10); do
        if ! ss -tln 2>/dev/null | grep -q ":${port} "; then
            break
        fi
        sleep 0.5
    done
    if ss -tln 2>/dev/null | grep -q ":${port} "; then
        echo "端口 ${port} 仍占用，强制释放..."
        fuser -k "${port}/tcp" 2>/dev/null || true
    fi
done

echo "已停止 ClipSync"
