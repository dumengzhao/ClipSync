#!/usr/bin/env python3
"""校验 Release 上的 `latest.json` 与**实际资产**是否逐字节一致。

## 为什么需要它

`latest.json` 里的 sha256 是 CI 构建时算出来的（来自构建产物），但 Release 上的
资产可能在之后被**覆盖**：典型场景是「在 GitHub 上点 Publish」时，若该 tag 的 ref
还不存在，GitHub 会在此刻才创建 tag —— 而创建 tag 属于 push 事件，会把 Release
工作流**再触发一次**；那次运行检出的是 **tag 指向的提交**，可能比你以为的旧，
于是用旧代码重建并覆盖掉已经好的资产。若那次运行中途失败（例如 Windows 上算
片段的那一步），就会留下：

    资产 = 新字节      latest.json = 旧哈希

结果：客户端与服务端的下载**全部**在 sha256 校验处失败 —— 而且**两边都是对的**，
坏的是发布产物本身。2026-09-24 实际踩到过（Release run #6）。

## 做法

不看构建产物，只看 Release 上**真实存在的那串字节**：把清单里每个平台引用的安装包
流式拉回来、边读边算 sha256，与清单声明的比对。不一致就让 CI 红掉。

草稿也能校验（走资产的 API 端点 + token，而不是 browser_download_url）。

用法：
    verify-release-manifest.py --repo owner/name --tag v0.3.2

退出码：0 = 一致（或没有可校验的对象）；1 = 不一致（CI 应当红）。
"""

import argparse
import hashlib
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request

# Windows / CI 控制台默认 cp1252，中文 print 会直接抛 UnicodeEncodeError 让脚本以
# 退出码 1 收场（2026-09-24 在 release.yml 的片段脚本上踩过同一个坑）。
try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
except Exception:  # noqa: BLE001 - 老解释器没有 reconfigure，忽略即可
    pass

API = "https://api.github.com"
UA = "clipsync-verify-release-manifest"


class _StripAuthOnCrossHostRedirect(urllib.request.HTTPRedirectHandler):
    """资产端点会 302 到签名过的临时 URL。

    把 Authorization 转发到另一个 host 会被 S3 拒（Only one auth mechanism
    allowed），所以跨 host 时把鉴权头摘掉。
    """

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        new = super().redirect_request(req, fp, code, msg, headers, newurl)
        if new is None:
            return None
        if urllib.parse.urlsplit(newurl).netloc != urllib.parse.urlsplit(req.full_url).netloc:
            for k in list(new.headers.keys()):
                if k.lower() == "authorization":
                    del new.headers[k]
            for k in list(new.unredirected_hdrs.keys()):
                if k.lower() == "authorization":
                    del new.unredirected_hdrs[k]
        return new


_OPENER = urllib.request.build_opener(_StripAuthOnCrossHostRedirect)


def _request(url, token, accept=None):
    headers = {"User-Agent": UA, "Accept": accept or "application/vnd.github+json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return urllib.request.Request(url, headers=headers)


def api_get_json(url, token):
    with _OPENER.open(_request(url, token), timeout=60) as r:
        return json.loads(r.read().decode("utf-8"))


def find_release(repo, tag, token):
    """按 tag 找 Release，找到返回 (release, None)，没有返回 (None, 'missing')。

    坑：`GET /releases/tags/{tag}` **查不到草稿** —— 草稿只出现在列表接口里
    （`GET /releases`，需 push 权限）。而本仓正常发版的流程是「先建草稿 → 人工
    Publish」，清单就是写进草稿的 ⇒ 只走 tags 端点会让本校验**在最该起作用的时候
    静默跳过**（2026-09-24 实测踩到）。所以 404 时回退到列表匹配。
    """
    try:
        return api_get_json(f"{API}/repos/{repo}/releases/tags/{urllib.parse.quote(tag)}", token), None
    except urllib.error.HTTPError as e:
        if e.code != 404:
            raise
    for page in range(1, 6):
        lst = api_get_json(f"{API}/repos/{repo}/releases?per_page=100&page={page}", token)
        if not isinstance(lst, list):
            break
        for r in lst:
            if r.get("tag_name") == tag:
                return r, None
        if len(lst) < 100:
            break
    return None, "missing"


def stream_sha256(url, token):
    """流式下载并算 sha256，返回 (hex, 字节数)。不落盘 —— AppImage 有 80+ MB。"""
    h = hashlib.sha256()
    n = 0
    with _OPENER.open(_request(url, token, accept="application/octet-stream"), timeout=900) as r:
        while True:
            chunk = r.read(1 << 20)
            if not chunk:
                break
            h.update(chunk)
            n += len(chunk)
    return h.hexdigest(), n


def fetch_json_bytes(url, token):
    with _OPENER.open(_request(url, token, accept="application/octet-stream"), timeout=120) as r:
        return r.read()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True, help="owner/name")
    ap.add_argument("--tag", required=True, help="形如 v0.3.2")
    ap.add_argument("--token", default=os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN") or "")
    args = ap.parse_args()

    if not args.token:
        print("::error::缺少 GH_TOKEN / GITHUB_TOKEN —— 无法读取草稿 Release 的资产")
        return 1

    try:
        rel, missing = find_release(args.repo, args.tag, args.token)
    except urllib.error.HTTPError as e:
        print(f"::error::读取 Release 失败：HTTP {e.code}")
        return 1
    if rel is None:
        print(f"::notice::tag {args.tag} 上还没有 Release（{missing}），跳过一致性校验")
        return 0

    assets = {a["name"]: a for a in rel.get("assets", [])}
    kind = "草稿" if rel.get("draft") else ("预发布" if rel.get("prerelease") else "已发布")
    print(f"Release {args.tag}（{kind}）：共 {len(assets)} 个资产")

    manifest_asset = assets.get("latest.json")
    if manifest_asset is None:
        print("::notice::Release 上没有 latest.json —— 跳过一致性校验（尚未发布过清单？）")
        return 0

    raw = fetch_json_bytes(manifest_asset["url"], args.token)
    try:
        manifest = json.loads(raw.decode("utf-8"))
    except Exception as e:  # noqa: BLE001
        print(f"::error::latest.json 不是合法 JSON：{e}")
        return 1

    platforms = manifest.get("platforms") or {}
    if not platforms:
        print("::error::latest.json 里没有任何平台条目")
        return 1

    print(f"清单声明版本 {manifest.get('version')} / {len(platforms)} 个平台")
    print()

    ok, bad = [], []
    for plat in sorted(platforms):
        entry = platforms[plat] or {}
        url = entry.get("url") or ""
        want = (entry.get("sha256") or "").lower()
        name = url.rsplit("/", 1)[-1]
        if not name:
            bad.append(f"{plat}: 清单里没有可解析的 url")
            continue
        asset = assets.get(name)
        if asset is None:
            bad.append(f"{plat}: 清单引用的 {name} 在 Release 上不存在")
            continue
        try:
            got, size = stream_sha256(asset["url"], args.token)
        except Exception as e:  # noqa: BLE001
            bad.append(f"{plat}: 下载 {name} 失败（{e}）")
            continue
        if got == want:
            ok.append(f"{plat:<16} {name}  {size} 字节  一致")
        else:
            bad.append(
                f"{plat}: {name} sha256 不一致 —— 清单 {want} / 实际 {got}（{size} 字节）"
            )

    for line in ok:
        print(f"  ✓ {line}")
    if bad:
        print()
        for line in bad:
            print(f"  ✗ {line}")
            print(f"::error::清单与资产不一致 —— {line}")
        print()
        print(
            "::error::latest.json 与 Release 上的实际资产不一致 —— 客户端与服务端的 "
            "sha256 校验都会失败（两边行为都是对的，坏的是发布产物本身）。"
            "修复：手动重跑 Release（workflow_dispatch，tag 填同一个），"
            "让它重新构建并重新生成清单；跑完本校验会自动复核。"
        )
        return 1

    print()
    print(f"✓ {len(ok)} 个平台全部一致")
    return 0


if __name__ == "__main__":
    sys.exit(main())
