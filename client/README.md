# ClipSync Client

终端应用模块 - 基于 Tauri + Rust 的跨平台桌面剪贴板同步工具。

## 目录结构

```
client/
├── src/                  # 前端 React + TypeScript
├── src-tauri/            # Rust 后端
│   ├── src/
│   │   ├── clipboard/    # 剪贴板抽象 + 三平台实现
│   │   ├── transfer/     # WebSocket + TCP 传输
│   │   ├── discovery/    # mDNS + 手动地址
│   │   ├── crypto/       # AES-GCM + X25519 + SPAKE2
│   │   ├── sync/         # 同步引擎 + 防回环 + Lamport 时钟
│   │   ├── device/ config/ cache/ update/ obs/
│   ├── Cargo.toml
│   └── tauri.conf.json
├── package.json
├── vite.config.ts
└── index.html
```

## 开发

```bash
cd client
npm install
npm run tauri dev
```

详细架构见 [docs/development-plan.md](../docs/development-plan.md)。
