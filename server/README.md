# ClipSync Server

跨局域网中转服务端（**Rust**，单二进制部署）。完整方案见 [架构方案](../docs/server-architecture-plan.md)。

## 角色

- 所有设备常连一条 WebSocket（Token 鉴权 + 心跳）。
- 维护 **Network** 注册表（`data/networks.json`，文件存储，无数据库）。
- **两步信任**：设备填 Token 连接 → 记为「未启用(pending)」→ 管理员登录管理页「启用」→ 才完成双向信任、参与同步。
- 跨局域网**文字**：服务端做密文中转（端到端加密，服务端看不到明文）。
- 跨局域网**文件**：服务端仅广播「待复制」通知（manifest + 对端对外 `ip:port`），字节由对端直取，服务端不碰。
- 内置**管理页面**（需登录）：网络管理、设备审批启用/禁用、Token 重置。

## 技术栈

`tokio` + `axum` + `tokio-tungstenite` + `rustls`（无 openssl 系统依赖）+ `argon2` + `sha2` + `serde` + `rust-embed`（管理页打包进二进制）。

## 构建与部署

```bash
cd server
cargo build --release --target x86_64-unknown-linux-musl   # 静态单二进制
./target/x86_64-unknown-linux-musl/release/clipsync-server
```

前置 nginx 终止 TLS 并反代 `/ws`、`/api/admin`、`/admin`。可选 `Dockerfile` + `docker-compose.yml`。

## 状态

方案 v2 已确认（Rust / 管理页+登录 / 文件存储 / 两步信任启用）。进入分阶段实现，见架构方案第 7 节路线图。

### 阶段 1（已完成 ✅）：服务端骨架 + 文件存储 + 设备 WS + 中继门控

- 设备 WS `/ws`：`auth`(Token 哈希校验) → 注册/更新节点（首次 `enabled=false`/pending）→ `welcome`；`heartbeat`；`relay_text`/`file_notify` 仅当源与目标均 `enabled=true` 且跨 `lan_group` 才转发（密文透传 / 仅广播 manifest）。
- 管理 REST（`Bearer ADMIN_API_KEY`，阶段2 将替换为登录会话）：`POST /api/admin/networks`（返回一次性 Token）、`GET /api/admin/networks`、设备列表、`enable`/`disable`（触发 `activated`/`deactivated` + `nodes_update`）。
- 文件存储：`data/networks.json`(Token 仅存哈希) / `data/admin.json`(argon2) / `data/server.key`，原子写 + 启动恢复。
- 联调脚本已验证：pending→启用→跨 LAN 中继→禁用停发 全部通过。

#### 本地运行（阶段1）

```bash
cd server
cargo build                 # 或 cargo build --release
CLIPSYNC_DATA_DIR=./data \
ADMIN_API_KEY=你的密钥 \
ADMIN_PASS=你的密码 \
LISTEN=0.0.0.0:24682 \
./target/debug/clipsync-server
```

- 健康检查：`GET /healthz`
- 创网络（拿 Token）：`curl -H "Authorization: Bearer $ADMIN_API_KEY" -d '{"name":"默认","description":""}' http://127.0.0.1:24682/api/admin/networks`
- 设备接入：`ws://host:24682/ws`，首条消息 `{"type":"auth","token":"<Token>","device":{"id":"...","name":"...","lan_group":"g1","ext_file_ep":"ip:port","platform":"mac"}}`

> 注：阶段1 管理 API 用 `ADMIN_API_KEY` 简易鉴权，登录页面 + 会话 Cookie/JWT 在阶段2 实现。
