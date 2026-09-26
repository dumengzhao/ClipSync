# ClipSync

跨平台剪贴板同步工具，基于 Tauri + Rust 实现。支持 Windows / macOS / Linux 三端文本、图片、文件实时同步，采用文件延迟渲染技术提供接近原生的复制粘贴体验。

## 特性

- **跨平台统一体验**：Windows、macOS、Linux 三端功能一致
- **文件延迟渲染**：大文件复制零等待，粘贴时按需传输
- **纯 P2P 直连**：终端即收即发，局域网 mDNS 自动发现，跨网手动配置地址
- **端到端加密**：AES-256-GCM + X25519 + SPAKE2 配对
- **极低资源占用**：包体积 < 15MB，内存 < 50MB

## 仓库结构

```
ClipSync/
├── client/             # 终端应用（Tauri + Rust 桌面端）
├── server/             # 跨局域网中转服务端（Rust，单二进制部署）
├── docs/               # 开发方案文档
├── .github/workflows/  # CI/CD（ci/nightly/release/security）
├── scripts/            # 辅助脚本
└── rust-toolchain.toml # 锁定 Rust 版本
```

终端应用在 `client/`，跨局域网中转服务端在 `server/`（部署方式见下方「服务端部署」），其他模块按需新增。

## 开发

### 环境要求

- Rust 1.75+（由 `rust-toolchain.toml` 自动锁定）
- Node.js 20+
- 系统依赖：
  - **macOS**：Xcode Command Line Tools
  - **Windows**：MSVC Build Tools + WebView2
  - **Linux**：`libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev patchelf`

### 启动开发

```bash
cd client
npm install
npm run tauri dev
```

### 本地 CI 验证

提交前运行（在仓库根目录）：

```bash
scripts/local-ci.sh
```

等价于 GitHub Actions 的 `ci.yml` 检查项：fmt、clippy、test、lint、build。

## 本地打包

### macOS

生成应用与磁盘镜像（macOS ad-hoc 签名；自动更新走无签名自托管方案，因此**不需要** Tauri updater 的签名密钥）：

```bash
cd client
npm run tauri build -- --bundles app
```

> 需 `dangerouslyDisableSandbox` 类真实环境（codesign / Keychain 写入被沙箱拦截）。

**打包产物目录（绝对路径）**：

```
<仓库>/client/src-tauri/target/release/bundle/macos/
├── ClipSync.app                 # 正式签名客户端（双击/拖到 Applications 即可运行）
└── ClipSync_0.1.0_aarch64.dmg   # 已签名磁盘镜像（可直接分发；版本/架构按下方规范命名）
```

> **签名钥匙串注意**：自签名证书 `ClipSync Dev` 同时存在于 `login.keychain-db` 与密码已遗失的 `build.keychain-db`。打包前必须把钥匙串搜索顺序调成 `login` 置首，否则 codesign 会卡在未知密码的 `build` 钥匙串（报 `errSecInternalComponent`）：
> ```bash
> security list-keychains -s ~/Library/Keychains/login.keychain-db ~/.clipsync/build.keychain-db /Library/Keychains/System.keychain
> ```
> `bundle_dmg.sh` 末段 `osascript` 在本机会被系统拦截（`-10004`），请改用下面的手工命令。
>
> **dmg 命名务必带版本与架构**（与 Tauri 官方产物一致，如 `ClipSync_0.1.0_aarch64.dmg`）。
> 管理后台上传时会从文件名自动识别版本号与平台架构，缺了这两段就只能手填：
> ```bash
> # 在仓库根执行
> V=$(node -pe "require('./client/src-tauri/tauri.conf.json').version")
> A=$(uname -m); [ "$A" = "arm64" ] && A=aarch64   # arm64→aarch64；x86_64 原样保留
> cd client/src-tauri/target/release/bundle/macos
> hdiutil create -volname ClipSync -srcfolder ClipSync.app -ov -format UDZO "ClipSync_${V}_${A}.dmg"
> codesign --sign "ClipSync Dev" "ClipSync_${V}_${A}.dmg"
> ```
> 注：macOS 的 `.app` 目录名固定为 `ClipSync.app`（不含版本/架构，架构只在包体 `lipo -archs` 里），
> 浏览器端无法解析，故分发请给 dmg 用上面的规范命名。

安装方式见下方「下载安装 → macOS」。

### Windows

生成 NSIS 安装包：

```bash
cd client
npm run tauri build -- --bundles nsis
```

产物（绝对路径）：

```
<仓库>/client/src-tauri/target/release/bundle/nsis/ClipSync_<版本>_x64-setup.exe
```

要点：

- **打包前先退出正在运行的客户端。** 否则安装包对应的可执行文件被占用，构建会以 `os error 5（拒绝访问）` 收场：
  ```bash
  taskkill /IM clipsync.exe /F
  ```
- **只发 NSIS。** MSI 安装会**绕过 `installer.nsh`**，而客户端的自更新入口正是靠它写入的标记文件
  （`NSIS_HOOK_POSTINSTALL` → `installed.marker`）来显示的 —— 用 MSI 装的客户端，设置页里的更新入口会消失。
- **不要绕过 Tauri CLI 直接 `cargo build --release`。** 生产构建需要 `tauri/custom-protocol`（`tauri` crate 的命名空间特性），
  CLI 会自动启用它；少了它客户端启动后是白屏。
- NSIS 工具链由 Tauri 在首次打包时**自动下载**，无需预先安装。
- `bundle/nsis/` 会**累积历史版本**的安装包，属构建产物，清理时注意别把规则从 `.gitignore` 里漏掉。

安装方式见下方「下载安装 → Windows」。

## 服务端部署（自托管）

服务端负责**跨局域网**场景：所有设备常连一条 WebSocket，做密文中转与设备审批，并可选地托管客户端更新包。
**局域网内直连（mDNS 自动发现）不需要服务端**，只有跨网、或想用「自动更新」时才需要一台。

> 完整说明（角色划分、技术栈、构建细节、Windows 交叉编译、musl 静态产物）见 **[server/README.md](server/README.md)**。

### 直接用现成产物（Linux，推荐）

从 [Releases](https://github.com/dumengzhao/ClipSync/releases) 下载 **`clipsync-server-linux.tar.gz`**，在 Linux 服务器上执行：

```bash
curl -LO https://github.com/dumengzhao/ClipSync/releases/latest/download/clipsync-server-linux.tar.gz
tar xzf clipsync-server-linux.tar.gz
cd clipsync-server-linux
sudo ./install.sh
```

脚本会输出随机管理员密码，请记下。配置见下一节。

### 从源码构建后再部署

```bash
# 在仓库根目录
cd server
bash package.sh     # 生成 server/dist/clipsync-server-linux/（二进制 + 脚本 + 配置样例）
```

把该目录**整目录**拷到服务器，然后同样 `cd clipsync-server-linux && sudo ./install.sh`。

Windows 上则在 `server/` 目录执行 `cargo build --release`，再用管理员 PowerShell 运行 `install.ps1`，
把 `clipsync-server.exe` 注册成 Windows 服务（开机自启、后台运行）。

### 配置

服务端的**启动配置**都是环境变量，集中写在 **`/opt/clipsync-server/clipsync-server.env`**（systemd 通过 `EnvironmentFile=` 读取），
样例见 [`server/clipsync-server.env.example`](server/clipsync-server.env.example)。改完重启生效：

```bash
sudo systemctl restart clipsync-server
```

（少数运行期开关——例如 GitHub 同步的启停与**定时表达式**——由管理页写入数据目录，改完即时生效，不用重启。）

常用项：

| 变量 | 说明 |
|---|---|
| `CLIPSYNC_DATA_DIR` | 数据目录（`networks.json`、密钥、更新包、管理员凭据 `admin.json`） |
| `LISTEN` | 监听地址，默认 `0.0.0.0:20070` |
| `UPDATE_PUBLIC_BASE` | 生成更新包下载链接用的公开基址，**生产建议显式设置**（如 `https://sync.example.com`） |
| `TRUSTED_PROXIES` | 反向代理与源站**不在同一台机器**时填反代的出口 IP；不填会让管理登录退避退化成单桶 |
| `GITHUB_REPO` | 填 `owner/name` 启用「服务端从 GitHub 拉取更新包」——客户端所在网络连不上 GitHub 时的兜底。**也可在管理页「GitHub 同步」里填，页面配置优先** |
| `HTTPS_PROXY` | 服务器直连不通 GitHub 时的出网代理（也可在管理页配，见下） |

> `install.sh` **只在首次部署时**从样例生成 env 文件，**已存在则原样保留** —— 后续改配置请直接编辑该文件，重跑安装脚本不会带上你的改动。

### 日志

服务端日志**两路输出同一份内容**：

- **stdout** → 交给 systemd：`journalctl -u clipsync-server -f`（前台直接跑时打在终端上）；
- **文件** → `<数据目录>/logs/clipsync-server.log.YYYY-MM-DD`，**按天滚动、保留 14 天**，
  不用登上去敲 journalctl 也能 `tail -f`（日志目录可用 `CLIPSYNC_LOG_DIR` 改到 `/var/log/clipsync` 之类）。

时间戳是**服务器本地时区**（与 cron 表达式的口径一致）。级别用 `CLIPSYNC_LOG_LEVEL`（默认 `info`，
也兼容 `RUST_LOG`），排查问题时可临时开 `debug`。

定时同步的关键节点都会记日志，例如：

```
INFO GitHub 同步定时已启用：仓库=dumengzhao/ClipSync cron="0 3 * * *"（按服务器本地时区）下次运行=2026-09-27T03:00:00+08:00
INFO 定时触发同步（cron="0 3 * * *"）→ 下次运行 2026-09-28T03:00:00+08:00
INFO 已下载 windows-x86_64/ClipSync_0.3.2_x64-setup.exe（2916834 字节）
INFO 同步完成：dumengzhao/ClipSync 版本 0.3.2，3 个平台入库（本次合并 3 个）
```

失败会走 `WARN`/`ERROR`（含具体原因），表达式写坏时也会明确告诉你「本轮跳过 + 多久后重试」。

### 服务器连不上 GitHub 怎么办（代理）

服务端同步走系统 `curl`，所以代理有三种配法，**优先级从高到低**：

1. **管理页**（推荐，不用重启）：`GitHub 同步` 面板里**分开填**「代理地址」（`http://1.2.3.4:7890`）、
   「代理账号」、「代理口令」，点「保存设置」后按「测试连接」立刻看到通不通（会报 HTTP 码、耗时、
   GitHub 上的版本号）。地址与凭据由服务端拼合并做 URL 编码，**不用自己写 `%40`**；
   口令不回显（改口令时才填一次，留空 = 不改动）。
   `socks5` / `socks5h` = **域名交给代理解析**，服务器自己解不出 github.com 时用后者。
2. **环境变量**：在 `/opt/clipsync-server/clipsync-server.env` 加 `HTTPS_PROXY=http://...`，重启服务；
   curl 会自己读 `HTTPS_PROXY` / `https_proxy` / `ALL_PROXY`。
3. 都不配 = 直连。

代理口令不会回显：接口返回与日志里都是 `user:***@host`，真口令只存在服务端的配置文件里。

### 定时同步（cron）

「GitHub 同步」面板的定时开关用 **cron 表达式**（标准 5 段：`分 时 日 月 周`，**按服务器本地时区**）：

| 表达式 | 含义 |
|---|---|
| `0 3 * * *` | 每天 3:00（默认） |
| `*/30 * * * *` | 每半小时 |
| `0 4 * * 1` | 每周一 4:00 |

两次触发至少间隔 5 分钟（更密的会被拒绝，避免频繁打搅 GitHub 与浪费带宽）。保存后管理页会显示**下次运行时间**。
旧配置里的「每 N 分钟」会在读取时自动迁移成 cron。

### 客户端怎么连

- **未上 TLS**：客户端「服务器地址」填 `ip:20070`。
- **上了 TLS（推荐）**：用 nginx 反代终止 TLS（样例 [`server/nginx-clipsync.conf.example`](server/nginx-clipsync.conf.example)），
  客户端填 **`wss://你的域名`**（端口 443）。
  **不要填成 `ip:20070`** —— 那会绕过 TLS 直连明文端口。

### 管理后台

`https://你的域名/admin`（未上 TLS 时为 `http://ip:20070/admin`）：网络与设备审批（启用 / 禁用 / 移出）、
中继 Token 重置、客户端更新包上传与发布、**历史版本查看与下载**、从 GitHub 同步更新包。

「历史版本」会列出数据目录里所有落过盘的安装包（手动上传的与从 GitHub 同步下来的都在内），
按版本号分组、标注哪一个是当前线上版本，点包名可直接下载 —— 旧版本包不会随新版本发布被删掉。

### 公开下载页（无需登录）

`/downloads` 是一个**不需要登录**的下载页：显示当前线上版本（各平台安装包 + 更新说明），
点包名直接下载。同源的数据端点是 `GET /update/versions.json`（公开、只读）。

默认**只公开当前线上版本**——数据目录里往往还留着测试包和失败的版本，一开历史就都挂出去了。
需要连历史版本一起公开时，在管理页「CLIENT UPDATE → HISTORY」区勾选**「公开页也列出历史版本」**
（会有二次确认）；开关落在 `<UPDATE_DIR>/downloads.json`，配置缺失或损坏时一律按**关闭**处理。

上传 / 发布 / 删除仍然需要登录，公开面只有「看版本 + 下载」。

**首次访问需要初始化**：服务端不保存明文口令，也没有 `ADMIN_PASS` 之类的环境变量 —— 页面会要求你填入口令哈希串。
最省事的是在服务器上跑服务端自带的命令（交互输入、终端不回显，参数与运行时校验完全一致）：

```bash
/opt/clipsync-server/clipsync-server hash
```

也可以用三方工具生成，只需指定参数为 **argon2id / m=19456 KiB / t=2 / p=1**，输出 PHC 格式串即可。

想跳过页面直接把凭据写好也可以（`<CLIPSYNC_DATA_DIR>/admin.json`，权限 600）：

```bash
HASH=$(printf '%s\n%s\n' "$PW" "$PW" | /opt/clipsync-server/clipsync-server hash --stdin)
printf '{"user":"admin","pass_hash":"%s","updated_at":0}\n' "$HASH" > /opt/clipsync-server/data/admin.json
chmod 600 /opt/clipsync-server/data/admin.json
```

写入后再用真实口令登录一次，登得进去才说明这串哈希没贴错。之后可在右上角头像菜单里改密码
（改密码同样只写哈希）。忘记口令时删掉 `<CLIPSYNC_DATA_DIR>/admin.json` 并重启即可重新初始化。

## 项目结构详情

详细架构见 [docs/development-plan.md](docs/development-plan.md)。

## 下载安装

正式发布见 [Releases](https://github.com/dumengzhao/ClipSync/releases)。

### macOS

首次打开会提示「未验证开发者」，请：
1. 右键点击应用 -> 「打开」->「打开」
2. 或终端执行：`xattr -dr com.apple.quarantine /Applications/ClipSync.app`

### Windows

首次运行会看到 SmartScreen 警告，点击「更多信息」->「仍要运行」。

### Linux

AppImage 直接运行；deb/rpm 用对应包管理器安装。

## 许可证

MIT
