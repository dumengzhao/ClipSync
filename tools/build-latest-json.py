#!/usr/bin/env python3
"""把各平台片段汇总成 `latest.json`（服务端与客户端共用的更新清单）。

产出结构与服务端 `server/src/update.rs::UpdateManifest`、客户端
`client/src-tauri/src/update/mod.rs::LatestManifest` **完全一致**：

    {
      "version": "0.3.2",
      "notes": "...",
      "pub_date": "2026-09-23T15:00:00Z",
      "platforms": {
        "windows-x86_64": { "url": "https://github.com/<repo>/releases/download/<tag>/<file>",
                            "sha256": "<hex>" }
      }
    }

要点：
- url 用**带 tag 的下载地址**（不可变），而不是 `releases/latest/download/...`
  （后者会随新版本漂移，清单一旦被缓存就指错包）；
- 平台键必须落在白名单内 —— 与服务端同一个集合，防止手滑写进无法识别的键；
- 文件名只取 basename，拒绝 `..` / 路径分隔符（清单会被服务端与客户端直接使用）。

用法（在仓库根目录）：
    python3 tools/build-latest-json.py --repo owner/name --tag v0.3.2 \
        --fragments fragments --out latest.json --notes "..." 
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
from datetime import datetime, timezone

# 与服务端 update.rs 的 PLATFORMS 白名单保持一致
ALLOWED_PLATFORMS = {
    "windows-x86_64",
    "windows-aarch64",
    "darwin-x86_64",
    "darwin-aarch64",
    "linux-x86_64",
    "linux-aarch64",
}


def safe_basename(name: str) -> str:
    base = name.replace("\\", "/").split("/")[-1].strip()
    if not base or base in {".", ".."} or ".." in base:
        raise SystemExit(f"::error::非法产物文件名: {name!r}")
    return base


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True, help="owner/name")
    ap.add_argument("--tag", required=True, help="发布 tag，如 v0.3.2")
    ap.add_argument("--fragments", required=True, help="片段所在目录")
    ap.add_argument("--out", required=True, help="输出的 latest.json 路径")
    ap.add_argument("--notes", default="", help="发布说明（可选）")
    args = ap.parse_args()

    frag_dir = pathlib.Path(args.fragments)
    files = sorted(frag_dir.rglob("*.json"))
    if not files:
        raise SystemExit(f"::error::{frag_dir} 下没有任何平台片段，拒绝生成空清单")

    version = args.tag[1:] if args.tag.startswith("v") else args.tag
    platforms: dict[str, dict[str, str]] = {}
    for f in files:
        try:
            frag = json.loads(f.read_text(encoding="utf-8"))
        except json.JSONDecodeError as e:
            raise SystemExit(f"::error::片段 {f} 不是合法 JSON: {e}")
        platform = str(frag.get("platform", ""))
        if platform not in ALLOWED_PLATFORMS:
            raise SystemExit(f"::error::片段 {f} 的平台键不在白名单内: {platform!r}")
        digest = str(frag.get("sha256", "")).strip().lower()
        if len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
            raise SystemExit(f"::error::片段 {f} 的 sha256 非法: {digest!r}")
        name = safe_basename(str(frag.get("file", "")))
        platforms[platform] = {
            "url": f"https://github.com/{args.repo}/releases/download/{args.tag}/{name}",
            "sha256": digest,
        }

    manifest = {
        "version": version,
        "notes": args.notes,
        "pub_date": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": platforms,
    }
    out = pathlib.Path(args.out)
    out.write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(out.read_text(encoding="utf-8"))
    print(f"已写入 {out}（{len(platforms)} 个平台：{', '.join(sorted(platforms))}）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
