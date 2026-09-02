# ClipSync Agent Guide

> 本文件供所有 AI agent 在开始工作前阅读，了解应用现状与开发约定。
> 完整设计方案见 [docs/development-plan.md](docs/development-plan.md)。
> 更新模块方案见 [server/UPDATE_MODULE_PLAN.md](server/UPDATE_MODULE_PLAN.md)。

## 应用简介

ClipSync 是跨平台剪贴板同步工具，基于 Tauri v2 + Rust，支持 Windows / macOS / Linux 三端文本、图片、文件实时同步。核心亮点是文件延迟渲染（粘贴时才传输），端到端加密。

- **连接模式**：默认 P2P 直连（客户端监听 20071）；并提供一个**可选的自建中继服务**（`server/`，用于跨 NAT 转发信令/文本/文件通知）。中继与客户端自动更新管理合建在同一进程（见更新方案）。
- **仓库**：https://github.com/dumengzhao/ClipSync
- **许可**：MIT
- **当前阶段**：外壳 + 中继 server 已可用；核心同步链路（阶段一 MVP）仍在实现中。

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
│   │   │   ├── error.rs      # thiserror 类型化错误
│   │   │   ├── lib.rs        # 应用入口 + 托盘/窗口逻辑
│   │   │   └── tauri_cmd.rs  # 前端可调用命令
│   │   ├── Cargo.toml
│   │   └── tauri.conf.json
│   └── package.json
├── server/             # 中继服务 + 客户端更新管理（Rust axum，已实现并部署）
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

## 自动更新（方案已定，待实现）

- **方案**：无签名自托管。详见 `server/UPDATE_MODULE_PLAN.md`（commit `d4d5568` 起）。
- 中继 server 同时托管更新：`GET /update/latest.json` + `GET /update/files/:platform/:file`（公开读），`POST /api/admin/update`（admin 鉴权上传）。
- **客户端**：因 Tauri 内置 `updater` 插件**强制签名、无法关闭**，改为**自写更新器**（`check_update` / `download_update` / `install_update` + SHA256 完整性校验）。计划移除 `tauri.conf.json` 的 `updater` 插件（当前仍在，待删）。
- **信任模型**：自托管，信任锚 = 用户自己的中继服务器 + TLS；**不做 ed25519 签名**。更新地址必须取自用户配置的 relay 地址，不硬编码作者服务器。
- 因此 `tauri build` 不再需要 `TAURI_SIGNING_PRIVATE_KEY`（签名密钥生成脚本 `scripts/generate-update-key.sh` 当前已无用）。

## 未实现功能（stub 占位）

> 以下按 development-plan 的阶段划分，未包含上面「已实现」里的外壳/中继增强。核心同步链路仍待实现。

### 阶段一 MVP（最高优先级）
- ❌ 剪贴板读写与监听（三平台 `ClipboardProvider` 实现都是 stub）
- ❌ WebSocket 单通道（信令 + 文件分片复用）
- ❌ 手动地址连接
- ❌ SPAKE2 设备配对流程
- ❌ 同步引擎协调逻辑
- ❌ 防回环标记读写
- ❌ Tauri 命令实际逻辑（`get_device_id` / `get_paired_devices` 返回占位数据）

### 阶段二
- ❌ mDNS 自动发现
- ❌ 设备列表 UI
- ❌ 文件完整传输
- ❌ 图片同步
- ❌ 历史记录

### 阶段三/四/五（延迟渲染）
- ❌ Windows IStream + IDataObject
- ❌ macOS NSPasteboardItemDataProvider
- ❌ Linux X11/Wayland 延迟渲染
- ❌ 文件流式传输
- ❌ LRU 缓存

### 阶段六
- 🟡 自动更新：方案已定（无签名自托管），服务端路由 + 客户端自写更新器待实现（见上）

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

## 开发要求

### 必须遵守
- **平台特定代码**写到对应平台文件（`windows.rs` / `macos.rs` / `linux.rs`），通过 `cfg` 选择编译，禁止运行时平台判断做核心逻辑分支
- **新增依赖前**验证三平台支持，禁止使用 C 库依赖未提供预编译 binary 的 crate
- **错误处理**：对外 API 用 `thiserror`，内部可用 `anyhow`；禁止 `unwrap()` / `expect()` 在非测试代码中
- **commit message** 遵循 Conventional Commits（`feat:` / `fix:` / `refactor:` 等）
- **PR 前运行** `scripts/local-ci.sh` 确保通过 CI 检查

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
- **SPAKE2 配对**：防中间人，配对码 6 位数字 10 分钟有效
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

下一步应实现**阶段一 MVP** 的核心同步链路：

1. 用 `arboard` 实现文本读写与监听（三平台）
2. 防回环标记读写（自定义 MIME 格式）
3. WebSocket 信令通道（tokio-tungstenite）
4. 手动地址连接 UI
5. SPAKE2 设备配对最简流程
6. 同步引擎协调：监听 -> 防回环 -> 加密 -> 发送 -> 对端接收 -> 写入

详细设计见 [docs/development-plan.md](docs/development-plan.md) 第四、五、六、七章。
自动更新（阶段六）方案已定，可在核心同步就绪后并行推进（见 `server/UPDATE_MODULE_PLAN.md`）。
