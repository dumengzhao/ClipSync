#!/usr/bin/env bash
# 本地 CI 等价脚本，提交前运行确保通过 GitHub Actions 检查
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
CLIENT="$ROOT/client"

echo "[1/6] cargo fmt --check"
cargo fmt --all --manifest-path "$CLIENT/src-tauri/Cargo.toml" -- --check

echo "[2/6] cargo clippy"
cargo clippy --manifest-path "$CLIENT/src-tauri/Cargo.toml" --all-targets --all-features -- -D warnings

echo "[3/6] cargo test"
cargo test --manifest-path "$CLIENT/src-tauri/Cargo.toml" --all --all-features

echo "[4/6] npm lint"
cd "$CLIENT"
npm run lint --silent

echo "[5/6] npm build"
npm run build --silent

echo "[6/6] verify no-default-features"
cargo build --manifest-path "$CLIENT/src-tauri/Cargo.toml" --no-default-features

echo ""
echo "✓ All checks passed."
