#!/usr/bin/env bash
# macOS 本地 ad-hoc 签名测试脚本（不适用付费公证）
# 用于验证构建产物的完整性签名是否正常
set -euo pipefail

APP_PATH="${1:-src-tauri/target/release/bundle/macos/ClipSync.app}"

if [ ! -d "$APP_PATH" ]; then
    echo "Error: $APP_PATH not found"
    echo "Run 'npm run tauri build' first"
    exit 1
fi

echo "Verifying ad-hoc signature on: $APP_PATH"
codesign --verify --verbose=4 "$APP_PATH"

echo ""
echo "Displaying signature info:"
codesign -dv --verbose=4 "$APP_PATH"

echo ""
echo "✓ Ad-hoc signature valid"
echo ""
echo "Note: ad-hoc signature does NOT pass Gatekeeper without user bypass."
echo "Users must right-click -> Open, or run:"
echo "  xattr -dr com.apple.quarantine $APP_PATH"
