#!/usr/bin/env bash
#
# 打包 Linux 部署目录：把交叉编译出的二进制 + 所有部署文件汇总到一个文件夹，
# 整目录拷到服务器后 sudo ./install.sh 即可。产物在 server/dist/clipsync-server-linux/。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HERE/target/x86_64-unknown-linux-musl/release/clipsync-server"
OUT="$HERE/dist/clipsync-server-linux"

[ -f "$BIN" ] || {
  echo "找不到二进制：$BIN" >&2
  echo "先交叉编译：RUSTC_BOOTSTRAP=1 cargo build --release --target x86_64-unknown-linux-musl" >&2
  exit 1
}

rm -rf "$OUT"
mkdir -p "$OUT"

cp "$BIN"                              "$OUT/clipsync-server"
cp "$HERE/install.sh"                  "$OUT/"
cp "$HERE/restart.sh"                  "$OUT/"
cp "$HERE/clipsync-server.service"     "$OUT/"
cp "$HERE/clipsync-server.env.example" "$OUT/"
cp "$HERE/nginx-clipsync.conf.example" "$OUT/"
cp "$HERE/README.md"                   "$OUT/部署说明-README.md"

chmod +x "$OUT/clipsync-server" "$OUT/install.sh" "$OUT/restart.sh" 2>/dev/null || true

echo ">>> 打包完成: $OUT"
echo ">>> 整目录拷到服务器后: sudo ./install.sh"
ls -la "$OUT"
