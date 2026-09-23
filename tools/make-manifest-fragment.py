#!/usr/bin/env python3
"""生成**单个平台**的更新清单片段（供 release 工作流汇总成 latest.json）。

各平台构建 job 只知道自己那一个产物，因此各自产出一个
`{ "platform": ..., "file": ..., "sha256": ... }` 片段；
由 `build-latest-json.py` 汇总成服务端/客户端共用的 `latest.json`。

跨平台一律用 Python 的 hashlib 计算哈希 —— **刻意不用 shasum/sha256sum**：
前者来自 Perl（Windows 的 Git Bash 没有），后者 macOS 上没有，写错会静默产出空值。

用法（在仓库根目录）：
    python3 tools/make-manifest-fragment.py --platform windows-x86_64 \
        --search-dir client/src-tauri/target --out-dir client/src-tauri/target/manifest-fragment
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import sys

# 每个平台对应的**安装包**匹配模式（与服务端 update.rs 的 PLATFORMS 白名单同集合）。
# 只挑"用户会真正安装/运行"的那一个文件：Linux 用 AppImage（自包含、客户端更新器可直接拉起），
# macOS 用 dmg，Windows 用 NSIS setup.exe。
PLATFORM_PATTERNS: dict[str, str] = {
    "windows-x86_64": "*-setup.exe",
    "windows-aarch64": "*-setup.exe",
    "darwin-x86_64": "*.dmg",
    "darwin-aarch64": "*.dmg",
    "linux-x86_64": "*.AppImage",
    "linux-aarch64": "*.AppImage",
}


def sha256_of(path: pathlib.Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def pick_artifact(platform: str, search_dir: pathlib.Path) -> pathlib.Path:
    pattern = PLATFORM_PATTERNS[platform]
    found = [p for p in search_dir.rglob(pattern) if p.is_file()]
    if not found:
        print(
            f"::error::未在 {search_dir} 找到匹配 {pattern} 的产物（platform={platform}）",
            file=sys.stderr,
        )
        raise SystemExit(1)
    if len(found) > 1:
        # 正常情况只会有一个；多个时取最新的，并显式提示（避免静默挑错文件）
        print(f"::warning::匹配到 {len(found)} 个 {pattern}，取最新修改的那个：{found}")
        found.sort(key=lambda p: p.stat().st_mtime)
    return found[-1]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--platform", required=True, choices=sorted(PLATFORM_PATTERNS))
    ap.add_argument("--search-dir", required=True)
    ap.add_argument("--out-dir", required=True)
    args = ap.parse_args()

    artifact = pick_artifact(args.platform, pathlib.Path(args.search_dir))
    out_dir = pathlib.Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    fragment = {
        "platform": args.platform,
        "file": artifact.name,
        "sha256": sha256_of(artifact),
    }
    out = out_dir / f"{args.platform}.json"
    out.write_text(json.dumps(fragment, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"片段已写入 {out}：{fragment['file']} ({artifact.stat().st_size} 字节)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
