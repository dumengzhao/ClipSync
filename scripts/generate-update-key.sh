#!/usr/bin/env bash
# 生成 Tauri updater 签名密钥（免费 Ed25519）
# 输出公钥（写入 tauri.conf.json）和私钥（填入 GitHub Secret）
set -euo pipefail

CLIENT_DIR="$(dirname "$0")/../client"

if ! command -v npm &> /dev/null; then
    echo "Error: npm not found"
    exit 1
fi

echo "Generating Tauri updater key pair..."
echo "(You will be prompted for a password - this becomes TAURI_KEY_PASSWORD)"
echo ""

cd "$CLIENT_DIR"
npm run tauri -- signer generate -- -w ~/.tauri/clipsync.key

echo ""
echo "================================================"
echo "Next steps:"
echo "1. Copy the PUBLIC key (shown above) into:"
echo "   client/src-tauri/tauri.conf.json -> plugins.updater.pubkey"
echo ""
echo "2. Add the PRIVATE key to GitHub Secrets as TAURI_PRIVATE_KEY"
echo "   (settings -> secrets and variables -> actions)"
echo ""
echo "3. Add the PASSWORD you chose as TAURI_KEY_PASSWORD"
echo ""
echo "The private key file is saved at ~/.tauri/clipsync.key"
echo "================================================"
