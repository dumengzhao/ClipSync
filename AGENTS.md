# ClipSync Agent Guide

> 本文件供所有 AI agent 在开始工作前阅读，了解应用现状与开发约定。
> 完整设计方案见 [docs/development-plan.md](docs/development-plan.md)。

## 应用简介

ClipSync 是跨平台剪贴板同步工具，基于 Tauri v2 + Rust，支持 Windows / macOS / Linux 三端文本、图片、文件实时同步。核心亮点是文件延迟渲染（粘贴时才传输），纯 P2P 直连无中继服务器，端到端加密。

- **仓库**：https://github.com/dumengzhao/ClipSync
- **许可**：MIT
- **当前阶段**：阶段一 MVP 早期（项目骨架已就绪，核心同步未实现）

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
├── server/             # 服务端模块（规划中，当前为空）
├── docs/               # 开发方案文档
├── .github/workflows/  # CI/CD（ci/nightly/release/security）
├── scripts/            # 辅助脚本
└── rust-toolchain.toml # Rust stable，MSRV 1.85
```

## 已实现功能

### 应用外壳（可工作）
- ✅ **系统托盘**：剪贴板形状图标，左键切换窗口显示，右键菜单（显示/隐藏/退出）
- ✅ **macOS Dock 隐藏**：启动仅菜单栏运行，窗口显示时 Dock 出现，隐藏时 Dock 消失
- ✅ **窗口关闭拦截**：点 X 改为隐藏窗口，进程不退出
- ✅ **应用图标**：RGBA PNG + ICO + ICNS，三端可用
- ✅ **前端启动**：Vite + React + TypeScript，显示版本号

### 工程化
- ✅ **CI/CD**：4 个 GitHub Actions 工作流，三平台矩阵（Linux x64 / macOS ARM+x64 / Windows x64）
- ✅ **本地 CI 脚本**：`scripts/local-ci.sh` 等价验证
- ✅ **开发规范**：rustfmt / clippy / eslint / prettier 配置，`RUSTFLAGS="-D warnings"`
- ✅ **签名方案**：仅免费证书（macOS ad-hoc + Windows 不签名 + Tauri Ed25519 updater + SHA256 校验）
- ✅ **Tauri updater 插件**：已集成，公钥待生成填入

### Rust 代码骨架（部分实现）
- ✅ **模块结构**：11 个模块目录，符合方案文档第十章
- ✅ **Trait 抽象**：`ClipboardProvider` / `Transport` 接口已定义
- ✅ **强类型**：`DeviceId` / `SyncId` newtype，`FileMeta` 含 `mime_type`，`ClipboardContent`，`SyncMark`
- ✅ **错误类型**：`thiserror` 定义的 `ClipboardError` / `CryptoError` / `TransferError` / `SyncError`
- ✅ **Lamport 时钟**：`sync/conflict.rs` 完整实现
- ✅ **AES-256-GCM 加解密**：`crypto/aead.rs` 纯函数实现
- ✅ **HKDF 密钥派生**：`crypto/kdf.rs` 实现
- ✅ **macOS Keychain 集成**：`crypto/keystore.rs` 实际可读写
- ✅ **配置结构**：`AppConfig` 含 14 个字段，默认值合理

## 未实现功能（stub 占位）

按方案文档的阶段划分：

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
- ❌ 自动更新实际逻辑
- ❌ 日志轮转 / 崩溃上报
- ❌ 国际化、主题、黑白名单

## 开发命令

```bash
# 启动开发（在 client/ 目录）
cd client
npm install
npm run tauri dev

# 本地 CI 验证（在仓库根目录）
scripts/local-ci.sh

# 单独运行 Rust 检查（在 client/src-tauri/ 目录）
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all --all-features

# 生成 Tauri updater 签名密钥
scripts/generate-update-key.sh
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
- macOS 用 ad-hoc 签名，Windows 不签名，仅 Tauri updater 用免费 Ed25519
- 用户首次安装需手动绕过 Gatekeeper / SmartScreen，README 已说明

### 文件约定
- 配置文件：`kebab-case.json`
- Rust 模块：`snake_case.rs`
- 前端组件：`PascalCase.tsx`
- 禁止提交：`.env`、`*.p12`、`*.pfx`、`*.key`、`target/`、`node_modules/`

## 关键技术决策

- **Tauri v2**（非 v1）：tray-icon 内置，image-png feature 需显式启用
- **objc2 + icrate**（非 objc）：macOS AppKit 绑定，强类型 + 引用计数安全
- **mdns-sd**（非 mdns crate）：纯 Rust，无 C 依赖
- **SPAKE2 配对**：防中间人，配对码 6 位数字 10 分钟有效
- **Lamport 时钟**：不依赖系统时钟解决多设备冲突
- **BLAKE3**（非 SHA-256）：文件哈希，性能更好

## 不要做的事

- ❌ 不要添加中继服务器相关代码（项目明确不使用）
- ❌ 不要做 NAT 穿透（用户自行配置对端地址）
- ❌ 不要购买或集成付费签名证书
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
