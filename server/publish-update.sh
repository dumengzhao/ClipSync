#!/usr/bin/env bash
#
# 一键发布客户端更新（无签名自托管，见 UPDATE_MODULE_PLAN.md 第 6/10 节）。
# 流程：tauri build 出安装包 → 计算各平台 sha256 → 生成自定义 latest.json
#       （无 signature/pubkey，url 只填文件名，由服务端返回时按自身 origin 改写）
#       → 经 admin API 单请求原子上传（文件 + manifest，传齐才切换线上版本）。
#
# 前置：本机已装 NSIS（出 windows-x86_64 安装包）；其它平台产物若存在则自动带上。
# 用法：
#   ./publish-update.sh <admin_base> <user> <pass> [notes]
# 例：
#   ./publish-update.sh https://sync.example.com admin 'S3cret' '修复托盘设置最小化后打不开'
set -euo pipefail

ADMIN_BASE="${1:?用法: publish-update.sh <admin_base> <user> <pass> [notes]}"
ADMIN_USER="${2:?缺少管理用户名}"
ADMIN_PASS="${3:?缺少管理密码}"
NOTES="${4:-}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLIENT_DIR="$HERE/../client"

command -v node >/dev/null || { echo "需要 node（生成 manifest / 解析 JSON）" >&2; exit 1; }
command -v curl >/dev/null || { echo "需要 curl" >&2; exit 1; }

# ---- 版本号（与 tauri.conf.json / Cargo.toml 保持一致）----
VERSION=$(node -pe "JSON.parse(require('fs').readFileSync('$CLIENT_DIR/src-tauri/tauri.conf.json','utf8')).version")
PUB_DATE=$(date -u +%Y-%m-%dT%H:%M:%SZ)
echo ">>> ClipSync v$VERSION（$PUB_DATE）"

# ---- 构建 ----
echo ">>> tauri build（需 NSIS；缺失 msi/WiX 不影响更新链路）"
cd "$CLIENT_DIR"
npm run tauri build

BUNDLE="$CLIENT_DIR/src-tauri/target/release/bundle"
ARGS=()
add_platform() { # platform 安装包路径（存在才收）
  local p="$1" f="$2"
  if [ -n "$f" ] && [ -f "$f" ]; then
    ARGS+=("$p" "$f")
    echo ">>> 平台 $p <- $f"
  fi
}
add_platform windows-x86_64 "$(ls "$BUNDLE"/nsis/*-setup.exe 2>/dev/null | head -1 || true)"
add_platform darwin-aarch64 "$(ls "$BUNDLE"/dmg/*.dmg 2>/dev/null | head -1 || true)"
add_platform darwin-x86_64  "$(ls "$BUNDLE"/dmg/x64/*.dmg 2>/dev/null | head -1 || true)"
add_platform linux-x86_64   "$(ls "$BUNDLE"/appimage/*.AppImage 2>/dev/null | head -1 || true)"
if [ ${#ARGS[@]} -lt 2 ]; then
  echo "没有找到任何平台安装包，检查 $BUNDLE" >&2
  exit 1
fi

# ---- 生成自定义 latest.json（sha256 在此计算；url 仅文件名，服务端改写）----
MANIFEST=$(node -e '
const fs = require("fs"), crypto = require("crypto");
const [version, notes, pubDate, ...pairs] = process.argv.slice(1);
const platforms = {};
for (let i = 0; i < pairs.length; i += 2) {
  const p = pairs[i], f = pairs[i + 1];
  const h = crypto.createHash("sha256");
  h.update(fs.readFileSync(f));
  platforms[p] = { url: f.split(/[\\/]/).pop(), sha256: h.digest("hex") };
}
process.stdout.write(JSON.stringify({ version, notes, pub_date: pubDate, platforms }));
' "$VERSION" "$NOTES" "$PUB_DATE" "${ARGS[@]}")
echo ">>> latest.json: $MANIFEST"

# ---- 登录拿会话 token ----
TOKEN=$(curl -fsS -X POST "$ADMIN_BASE/api/admin/login" \
  -H 'Content-Type: application/json' \
  -d "$(node -e 'const[a,b]=process.argv.slice(1);process.stdout.write(JSON.stringify({user:a,pass:b}))' "$ADMIN_USER" "$ADMIN_PASS")" \
  | node -pe 'JSON.parse(require("fs").readFileSync(0,"utf8")).token')

# ---- 单请求上传：每组文件 platform/filename/file 依次，manifest 最后 ----
CURL_ARGS=( -fsS -X POST -H "Authorization: Bearer $TOKEN" )
i=0
while [ $i -lt ${#ARGS[@]} ]; do
  p="${ARGS[$i]}"; f="${ARGS[$((i+1))]}"; i=$((i+2))
  CURL_ARGS+=( -F "platform=$p" -F "filename=$(basename "$f")" -F "file=@$f;type=application/octet-stream" )
done
CURL_ARGS+=( -F "manifest=$MANIFEST;type=application/json" )

echo ">>> 上传到 $ADMIN_BASE/api/admin/update ..."
curl "${CURL_ARGS[@]}" "$ADMIN_BASE/api/admin/update"
echo
echo ">>> 发布完成。客户端设置页「检查更新」即可拉到 v$VERSION。"
