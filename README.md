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
├── server/             # 服务端模块（规划中，当前为空）
├── docs/               # 开发方案文档
├── .github/workflows/  # CI/CD（ci/nightly/release/security）
├── scripts/            # 辅助脚本
└── rust-toolchain.toml # 锁定 Rust 版本
```

终端应用在 `client/`，未来服务端在 `server/`，其他模块按需新增。

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

## 本地打包（macOS）

生成正式签名的应用与磁盘镜像：

```bash
cd client
TAURI_SIGNING_PRIVATE_KEY_PATH=~/.tauri/clipsync.key \
TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" \
npm run tauri build -- --bundles app
```

> 需 `dangerouslyDisableSandbox` 类真实环境（codesign / Keychain 写入被沙箱拦截）。

**打包产物目录（绝对路径）**：

```
/Volumes/ssd/DMZ/Work/DmzClipSync/client/src-tauri/target/release/bundle/macos/
├── ClipSync.app   # 正式签名客户端（双击/拖到 Applications 即可运行）
└── ClipSync.dmg   # 已签名磁盘镜像（4.54 MB，可直接分发）
```

> **签名钥匙串注意**：自签名证书 `ClipSync Dev` 同时存在于 `login.keychain-db` 与密码已遗失的 `build.keychain-db`。打包前必须把钥匙串搜索顺序调成 `login` 置首，否则 codesign 会卡在未知密码的 `build` 钥匙串（报 `errSecInternalComponent`）：
> ```bash
> security list-keychains -s ~/Library/Keychains/login.keychain-db ~/.clipsync/build.keychain-db /Library/Keychains/System.keychain
> ```
> `bundle_dmg.sh` 末段 `osascript` 在本机会被系统拦截（`-10004`），请改用：
> ```bash
> hdiutil create -volname ClipSync -srcfolder ClipSync.app -ov -format UDZO ClipSync.dmg
> codesign --sign "ClipSync Dev" ClipSync.dmg
> ```

安装方式见下方「下载安装 → macOS」。

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
