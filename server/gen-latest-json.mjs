#!/usr/bin/env node
/**
 * 生成客户端更新清单 latest.json（无签名自托管，见 UPDATE_MODULE_PLAN.md）。
 *
 * 用法：node gen-latest-json.mjs [notes]
 *
 * - 扫描 `tauri build` 产物（nsis / dmg / appimage），逐个算 sha256
 * - `url` 只填文件名，服务端返回时按自身 origin 改写（见 server/src/update.rs）
 * - 落盘到 `client/src-tauri/target/release/bundle/nsis/latest.json`
 * - stdout 只输出 JSON 本身（供 publish-update.sh 捕获），提示信息一律走 stderr
 *
 * 单独跑（只生成不上传）或由 publish-update.sh 复用，两条路共用同一份逻辑。
 */
import { readFileSync, writeFileSync, existsSync, readdirSync, statSync, mkdirSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const CLIENT_DIR = resolve(HERE, '../client');
const CONF = join(CLIENT_DIR, 'src-tauri/tauri.conf.json');
const BUNDLE = join(CLIENT_DIR, 'src-tauri/target/release/bundle');

const notes = process.argv[2] ?? '';

const version = JSON.parse(readFileSync(CONF, 'utf8')).version;
const pubDate = new Date().toISOString().replace(/\.\d{3}Z$/, 'Z');
console.error(`>>> v${version}（${pubDate}）`);

// 各平台产物：平台标识 + bundle 子目录 + 文件名正则
const CANDIDATES = [
  ['windows-x86_64', 'nsis', /-setup\.exe$/],
  ['darwin-aarch64', 'dmg', /\.dmg$/],
  ['darwin-x86_64', 'dmg/x64', /\.dmg$/],
  ['linux-x86_64', 'appimage', /\.AppImage$/],
];

const platforms = {};
for (const [platform, sub, re] of CANDIDATES) {
  const dir = join(BUNDLE, sub);
  if (!existsSync(dir)) continue;
  // 取「最近修改」的那个：目录里若残留旧版本安装包，按名字排序可能选中错的
  const hit = readdirSync(dir)
    .filter((f) => re.test(f))
    .map((f) => ({ f, t: statSync(join(dir, f)).mtimeMs }))
    .sort((a, b) => b.t - a.t)[0]?.f;
  if (!hit) continue;
  const file = join(dir, hit);
  platforms[platform] = {
    url: hit,
    sha256: createHash('sha256').update(readFileSync(file)).digest('hex'),
  };
  console.error(`>>> 平台 ${platform} <- ${file}`);
}

if (Object.keys(platforms).length === 0) {
  console.error(`没有找到任何平台安装包，先跑 npm run tauri build（检查 ${BUNDLE}）`);
  process.exit(1);
}

const manifest = JSON.stringify({ version, notes, pub_date: pubDate, platforms });

const out = join(BUNDLE, 'nsis', 'latest.json');
mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, manifest);
console.error(`>>> 已写入 ${out}`);

process.stdout.write(manifest);
