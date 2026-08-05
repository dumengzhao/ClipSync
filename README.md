# ClipSync

跨平台剪贴板同步工具，基于 Tauri + Rust 实现。支持 Windows / macOS / Linux 三端文本、图片、文件实时同步，采用文件延迟渲染技术提供接近原生的复制粘贴体验。

## 特性

- **跨平台统一体验**：Windows、macOS、Linux 三端功能一致
- **文件延迟渲染**：大文件复制零等待，粘贴时按需传输
- **纯 P2P 直连**：终端即收即发，局域网 mDNS 自动发现，跨网手动配置地址
- **端到端加密**：AES-256-GCM + X25519 + SPAKE2 配对
- **极低资源占用**：包体积 < 15MB，内存 < 50MB

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
npm install
npm run tauri dev
```

### 本地 CI 验证

提交前运行：

```bash
npm run local-ci
```

等价于 GitHub Actions 的 `ci.yml` 检查项：fmt、clippy、test、lint、build。

## 项目结构

```
clipsync/
├── docs/                # 开发方案文档
├── src/                 # 前端 React/TS
├── src-tauri/           # Rust 后端
├── .github/workflows/   # CI/CD
├── scripts/             # 辅助脚本
└── rust-toolchain.toml  # 锁定 Rust 版本
```

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
