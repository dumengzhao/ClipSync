# ClipSync Agent Guide

> 本文件供所有 AI agent 在开始工作前阅读，了解应用现状与开发约定。
> 完整设计方案见 [docs/development-plan.md](docs/development-plan.md)。
> 更新模块方案见 [server/UPDATE_MODULE_PLAN.md](server/UPDATE_MODULE_PLAN.md)。

## 应用简介

ClipSync 是跨平台剪贴板同步工具，基于 Tauri v2 + Rust，支持 Windows / macOS / Linux 三端文本、图片、文件实时同步，端到端加密。文件走「清单通告 + 按需拉取」：复制方只广播元数据，接收方拉取时才传字节（默认 1 MiB 以下自动拉取，以上等用户点）。**OS 级延迟渲染（IStream / NSPasteboardItemDataProvider）尚未实现**，当前用真实文件路径写入剪贴板。

- **连接模式**：默认 P2P 直连（客户端监听 20071）；并提供一个**可选的自建中继服务**（`server/`，用于跨 NAT 转发信令/文本/文件通知）。中继与客户端自动更新管理合建在同一进程（见更新方案）。
- **仓库**：https://github.com/dumengzhao/ClipSync
- **许可**：MIT
- **当前阶段**：外壳、中继 server、核心同步链路（三平台剪贴板、P2P 加密通道、SPAKE2 配对、mDNS 发现、文件按需拉取、跨 LAN 中继、无签名自更新）均已实现并在实机验证；仍缺 OS 级延迟渲染与剪贴板历史。

## 仓库结构

```
ClipSync/
├── client/             # 终端应用（Tauri + Rust 桌面端）
│   ├── src/            # React + TypeScript 前端
│   ├── src-tauri/      # Rust 后端
│   │   ├── src/
│   │   │   ├── clipboard/    # ClipboardProvider trait + 三平台实现
│   │   │   ├── transfer/     # WebSocket + TCP 传输
│   │   │   ├── discovery/    # mDNS + 手动地址
│   │   │   ├── crypto/       # AES-GCM + X25519 + SPAKE2
│   │   │   ├── sync/         # 同步引擎 + 防回环 + Lamport 时钟
│   │   │   ├── device/ config/ cache/ update/ obs/
│   │   │   ├── server_conn.rs # 跨 LAN 中继客户端（鉴权 + 网络密钥 + 拉取选路）
│   │   │   ├── file_server.rs # 复用 20071 的跨 LAN 文件直取端点（GET /file/<hash>）
│   │   │   ├── file_share.rs  # 已复制文件登记表（hash → 本地路径）
│   │   │   ├── outbox.rs      # 有界队列的载荷投递（背压语义，P2P/中继共用）
│   │   │   ├── log_viewer.rs  # 实时日志窗口（动态建窗 + tail）
│   │   │   ├── error.rs      # thiserror 类型化错误
│   │   │   ├── lib.rs        # 应用入口 + 托盘/窗口逻辑
│   │   │   └── tauri_cmd.rs  # 前端可调用命令
│   │   ├── Cargo.toml
│   │   └── tauri.conf.json
│   └── package.json
├── server/             # 中继服务 + 客户端更新管理（Rust axum，已实现；产物已构建待部署）
│   ├── src/            # main / hub / ws / admin / admin_ws / crypto / models / state / storage
│   ├── static/         # 内嵌管理页
│   ├── UPDATE_MODULE_PLAN.md  # 无签名自托管更新方案
│   ├── clipsync-server.env.example / .service / install.sh / nginx-clipsync.conf.example
│   └── package.sh      # 交叉编译为 Linux musl 静态单目录分发
├── docs/               # 开发方案文档
├── .github/workflows/  # CI/CD（ci/nightly/release/security）
├── scripts/            # 辅助脚本
└── rust-toolchain.toml # Rust stable，MSRV 1.85
```

## 已实现功能

### 应用外壳（可工作）
- ✅ **系统托盘**：剪贴板形状图标，左键切换窗口显示，右键菜单（显示/隐藏/退出）
- ✅ **托盘「设置」修复**：打开过主界面并最小化到任务栏后，点托盘「设置」会先 `unminimize()` 再 `show()`+`set_focus()` 还原窗口（commit `c4a0b80`）
- ✅ **macOS Dock 隐藏**：启动仅菜单栏运行，窗口显示时 Dock 出现，隐藏时 Dock 消失
- ✅ **窗口关闭拦截**：点 X 改为隐藏窗口，进程不退出
- ✅ **应用图标**：RGBA PNG + ICO + ICNS，三端可用
- ✅ **前端启动**：Vite + React + TypeScript，显示版本号
- ✅ **默认深色主题**（commit `fa42a84`）
- ✅ **窗口尺寸设置**：设置页可配置默认宽高，`get_window_size` 命令 + 启动时应用/持久化（`4ceb22a`/`b688347`）

### 中继服务（已实现并部署）
- ✅ **Rust axum 中继**：信令/文本/文件通知转发，跨 NAT 可用；纯 Rust 无 C 依赖，可交叉编译为 Linux musl 静态二进制
- ✅ **部署**：已部署到公网腾讯云，默认监听 `20070`（管理 `/admin`、健康检查 `/healthz`、WebSocket `/ws`）
- ✅ **管理页**：内嵌 `rust-embed` 管理页 + `admin_auth`（JWT）
- ✅ **独立 Windows 服务模式**：`--service` 走 SCM（`windows-service`），含 `install.ps1`/`uninstall.ps1`/`installer.nsi`

### 工程化
- ✅ **CI/CD**：4 个 GitHub Actions 工作流，三平台矩阵（Linux x64 / macOS ARM+x64 / Windows x64）
- ✅ **本地 CI 脚本**：`scripts/local-ci.sh` 等价验证
- ✅ **开发规范**：rustfmt / clippy / eslint / prettier 配置，`RUSTFLAGS="-D warnings"`
- ✅ **签名约定（见下）**：不买付费证书；macOS ad-hoc，Windows 不签名；**自动更新不做签名**

### Rust 代码骨架（部分实现）
- ✅ **模块结构**：11 个模块目录，符合方案文档第十章
- ✅ **Trait 抽象**：`ClipboardProvider` / `Transport` 接口已定义
- ✅ **强类型**：`DeviceId` / `SyncId` newtype，`FileMeta` 含 `mime_type`，`ClipboardContent`，`SyncMark`
- ✅ **错误类型**：`thiserror` 定义的 `ClipboardError` / `CryptoError` / `TransferError` / `SyncError`
- ✅ **Lamport 时钟**：`sync/conflict.rs` 完整实现
- ✅ **AES-256-GCM 加解密**：`crypto/aead.rs` 纯函数实现
- ✅ **HKDF 密钥派生**：`crypto/kdf.rs` 实现
- ✅ **macOS Keychain 集成**：`crypto/keystore.rs` 实际可读写
- ✅ **配置结构**：`AppConfig` 含默认窗口宽高字段；存储已迁移到用户目录 `~/ClipSync`（`03dc866` 配置 / `d46d3d0` 配对设备）

## 自动更新（已实现）

- **方案**：无签名自托管。详见 `server/UPDATE_MODULE_PLAN.md`（commit `d4d5568` 起）。
- 中继 server 同时托管更新：`GET /update/latest.json` + `GET /update/files/:platform/:file`（公开读），`POST /api/admin/update`（admin 鉴权上传）。
- **客户端**：因 Tauri 内置 `updater` 插件**强制签名、无法关闭**，改为**自写更新器**（`check_update` / `download_update` / `install_update` + SHA256 完整性校验）。`tauri.conf.json` 的 `updater` 插件**已移除**。
- **`install_update` 的参数绑定**：只接受 `AppState.pending_update` 里记录的（路径, sha256）——即本进程本次下载并校验通过的包，且启动前**复算哈希**。绝不可放宽为「接受前端传入的任意路径」：那等于给渲染器一个拉任意程序并退出主进程的入口。
- **信任模型**：自托管，信任锚 = 用户自己的中继服务器 + TLS；**不做 ed25519 签名**。更新地址必须取自用户配置的 relay 地址，不硬编码作者服务器。
- 因此 `tauri build` 不再需要 `TAURI_SIGNING_PRIVATE_KEY`；CI 也不再注入 `TAURI_PRIVATE_KEY` / `TAURI_KEY_PASSWORD`（密钥生成脚本 `scripts/generate-update-key.sh` 已删除）。

## 功能现状与剩余未实现项

### 已实现（曾经列在本节「stub 占位」里的项，均已完成）
三平台剪贴板读写与监听、WebSocket 单通道（信令 + 分片复用）、手动地址连接、SPAKE2 配对、同步引擎与防回环（内容哈希 + Lamport）、mDNS 自动发现、设备列表 UI、文件完整传输（边读边发 + 落盘平铺）、图片同步、流式传输、LRU + TTL 文件缓存（秒传）、跨 LAN 中继（文本/文件通知）、实时日志窗口、无签名自更新。

### 仍未实现
- ❌ **OS 级延迟渲染**：Windows `IStream` + `IDataObject`、macOS `NSPasteboardItemDataProvider`、Linux X11/Wayland 延迟写入（`clipboard/linux.rs` 的延迟写路径目前是 `bail!`）。当前实现是「真实路径 + CF_HDROP / 真实文件」，功能可用但不是延迟渲染。
- ❌ **剪贴板历史记录**：只有「当前剪贴板 + 待拉取清单」，没有历史列表与检索。
- 🟡 服务端产物已构建但**尚未部署**（见「当前待办」）。

## 开发命令

```bash
# 启动开发（在 client/ 目录）
cd client
npm install
npm run tauri dev          # 先确保 1420 端口空闲，否则 vite 启动失败

# 本地 CI 验证（在仓库根目录）
scripts/local-ci.sh

# 单独运行 Rust 检查（在 client/src-tauri/ 目录）
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all --all-features

# 中继服务（在 server/ 目录）
cd server
cargo build --release                         # Windows 本机可执行 clipsync-server.exe
# 交叉编译 Linux musl 静态单目录：
RUSTC_BOOTSTRAP=1 package.sh                   # 产出 server/dist/clipsync-server-linux/
```

Rust 工具链由 `rust-toolchain.toml` 自动锁定为 stable（MSRV 1.85）。

**CI 门禁**（`.github/workflows/ci.yml`，三平台矩阵 ubuntu-22.04 / macos-latest / windows-latest）：
`cargo fmt --all -- --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test --all --all-features`、`npm run lint`。改完先本地跑一遍 `scripts/local-ci.sh`。

**发版**：推 `v*` 标签，或在 Actions 里**手动触发** `Release` 工作流（输入形如 `v0.3.2` 的 tag；手动触发时 `github.ref_name` 是分支名，所以必须显式给 tag）。两个入口都会先跑 `verify-version` job：**tag 必须与 `tauri.conf.json` / `src-tauri/Cargo.toml` / `client/package.json` 的 version 一致**，不一致则打包前直接失败。产出是**草稿** Release，需手动 Publish；客户端真正收到更新还要在服务端管理页上传安装包并生成 `latest.json`。

**曾有的环境限制（已解决）**：Windows 本机一度跑不动客户端单测——`cargo test --lib` 以
`0xc0000139 STATUS_ENTRYPOINT_NOT_FOUND` 退出。根因是 `rfd` 静态导入仅 ComCtl32 v6 提供的
`TaskDialogIndirect`，而 cargo 链接**测试目标**时不嵌入 v6 依赖；已在 `client/src-tauri/build.rs`
用 `MANIFESTDEPENDENCY` 补上（commit `c9eaefb`），现在本机可直接 `cargo test --lib`。
排查特征：**「cargo test 报错但 clippy 通过」**（clippy 不做链接）。

## 开发要求

### 必须遵守
- **平台特定代码**写到对应平台文件（`windows.rs` / `macos.rs` / `linux.rs`），通过 `cfg` 选择编译，禁止运行时平台判断做核心逻辑分支
- **新增依赖前**验证三平台支持，禁止使用 C 库依赖未提供预编译 binary 的 crate
- **错误处理**：对外 API 用 `thiserror`，内部可用 `anyhow`；禁止 `unwrap()` / `expect()` 在非测试代码中
- **commit message** 遵循 Conventional Commits（`feat:` / `fix:` / `refactor:` 等）
- **PR 前运行** `scripts/local-ci.sh` 确保通过 CI 检查

### 安全与架构约定（改代码前必读）
这些都是踩过坑之后定下的，改动时不要绕过：
- **密钥不进渲染器**：本机 `network_token` 在 `get_config` 里换成哨兵 `__clipsync_token_unchanged__`（`set_config` 见哨兵即保持现值）；「局域网配置复制」返回的是脱敏摘要（`group_id` + 掩码），明文只存 Rust 侧缓存（TTL 300s、取用即消费），由 `apply_lan_server_config(group_id)` 在后端应用。
- **配置写入走单一入口**：所有「写配置」必须经 `tauri_cmd::apply_config`（校验 → 落盘 → 同步 mDNS/自启/手动地址簿/服务端重连），别另写一份。命令参数（路径/URL/地址）一律视为不可信。
- **命令参数绑定信任边界**：`install_update` 只认本进程下载记录（路径 + 复算 sha256）；`probe_ext_file_ep` / `ext_file_ep` 只用 `server_conn::ext_file_ep_is_valid` 认可的 `host[:port]` 形态。
- **载荷必须走背压**：剪贴板内容/文件清单/拉取请求等载荷用 `crate::outbox::send_payload`（满队列等待 + 超时）；只有心跳/通知类控制消息可 `try_send` 丢弃。**广播绝不写 `for … await`**（用 `manager::broadcast_payload` 并发，单个卡死对端才能不拖累其它对端）。
- **关键信号不用有界队列**：断连走 `Peer.close: Arc<Notify>`，不要用 `try_send(Outgoing::Close)`（队列满即静默失效）。
- **限速的键用纯 IP**（`IP:port` 每次连接都不同 → 永不累积 → 形同虚设）。
- **对不可信输入**：字符串截取用 `chars().take(n)`（禁用 `&s[..n]`，非 ASCII 会 panic）；超时给**总时限**（`timeout_at`）而非单次 IO 限时；Windows 剪贴板扫描以 `GlobalSize` 为界（NUL 终结不可信）；对端可控字符串进日志前过 `obs::logging::log_safe`。
- **上限类配置必须真的被读**：`max_file_size_mb` / `max_image_size_mb` 曾长期是纯展示字段（全仓零引用），新增此类配置要同时写清读写点。
- **CSP 非 null**：前端新增内联脚本/外部资源前先确认 CSP（`connect-src` 已含 `ipc:` 与 `http://ipc.localhost`），改完必须实测 IPC 与渲染。

### 签名约定
- **不购买付费证书**：不上架 App Store，不买 Apple Developer ID，不买 Windows EV 证书
- macOS 用 ad-hoc 签名，Windows 不签名
- **自动更新不做签名**：自托管，信任锚 = 中继服务器 + TLS；下载后用 `SHA256` 校验完整性
- 用户首次安装需手动绕过 Gatekeeper / SmartScreen，README 已说明

### 文件约定
- 配置文件：`kebab-case.json`
- Rust 模块：`snake_case.rs`
- 前端组件：`PascalCase.tsx`
- 禁止提交：`.env`、`*.p12`、`*.pfx`、`*.key`、`target/`、`node_modules/`、`server/dist/`（`dist/` 由 `package.sh` 生成，已 gitignore）

## 关键技术决策

- **Tauri v2**（非 v1）：tray-icon 内置，image-png feature 需显式启用
- **objc2 + icrate**（非 objc）：macOS AppKit 绑定，强类型 + 引用计数安全
- **mdns-sd**（非 mdns crate）：纯 Rust，无 C 依赖
- **SPAKE2 配对**：配对码是 **12 位 base32（60 bit）常驻值**（每台设备一个，首配对时由发起方输入「对端界面上显示的码」；重连走 link secret 不再用它）。曾用 6 位数字：确认标签是会话密钥的确定性函数，**谁先发标签谁就把低熵口令暴露成可离线穷举的 oracle**，故现在 (a) 确认必须**有序**——应答方先核对、通过后才出证，不匹配只回 Reject；(b) 失败限速按**纯 IP** 指数退避（`IP:port` 每次连接都变，等于不限速）。
- **Lamport 时钟**：不依赖系统时钟解决多设备冲突
- **BLAKE3**（非 SHA-256）：文件哈希，性能更好
- **自建中继 server（Rust axum + rustls）**：与客户端更新管理合建同一进程；纯 Rust 无 C 依赖，可交叉编译为 Linux musl 静态二进制；20070 中继 / 20071 客户端 P2P
- **自动更新无签名**：自托管，服务端 serve latest.json + 安装包，客户端自写更新器，移除 Tauri `updater` 插件（其强制签名无法关闭）

## 不要做的事

- ❌ 不要购买或集成付费签名证书
- ❌ 不要给自动更新加签名（已拍板无签名自托管；信任锚 = 服务器 + TLS）
- ❌ 不要把更新检查 URL 硬编码成作者公网服务器（必须取自用户配置的 relay 地址）
- ❌ 不要做 P2P NAT 穿透（跨 NAT 走自建中继 server）
- ❌ 不要在剪贴板回调中 `panic!`（会崩溃整个程序）
- ❌ 不要在日志中记录剪贴板内容（隐私敏感）
- ❌ 不要跳过 hooks（`--no-verify`）提交

## 当前待办

核心同步链路已完成，剩余事项按优先级：

1. **部署服务端**：`server/dist/clipsync-server-linux/` 已构建（scp + `install.sh`），线上仍是旧逻辑。
   部署前先确认 `ADMIN_PASS` 不是默认值（新版本会拒绝弱口令启动）。
2. **提交并推一次 CI**：客户端单测在本机不可执行（见上），推上去让三平台矩阵真实跑一遍。
3. **版本与安装包**：0.2.0 之后有一批**破坏性**变更（配对码格式、握手确认顺序、CSP、大小上限生效），
   发布前决定新版本号并重打 NSIS 包（`cd client && npx tauri build`，仅发 NSIS）。
4. **OS 级延迟渲染**（阶段三/四/五）：Windows `IStream`/`IDataObject`、macOS `NSPasteboardItemDataProvider`、
   Linux X11/Wayland 延迟写入。
5. **剪贴板历史记录**（可选）。

详细设计见 [docs/development-plan.md](docs/development-plan.md)；自更新方案见 `server/UPDATE_MODULE_PLAN.md`。
