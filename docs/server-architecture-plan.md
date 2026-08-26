# ClipSync 服务端架构方案（跨局域网中转）

> 状态：方案稿 v2（已确认：Rust 服务端 + 管理页面/登录 + 文件存储 + 两步信任启用）
> 关联：本方案推翻 `server/README.md` 中「纯 P2P、不需要服务端」的旧规划。

## 1. 背景与目标

当前架构为纯 P2P：mDNS 同局域网直连 + 手动地址 + 两两配对，文字/文件均经直连 WS（AES-GCM）同步。
痛点：**不同局域网内的设备无法被发现、也无法同步**。

目标：引入一个常驻**服务端**作为跨局域网中枢，同时**保留局域网直连不变**：
- 同局域网：继续 mDNS 直连（现有逻辑不动）。
- 跨局域网：文字经服务端中转；文件由对端对外 `ip:port` 直取，服务端仅通知、不碰字节。

## 2. 已确认的关键决策（用户拍板）

| 决策点 | 结论 |
|---|---|
| 服务端技术栈 | **Rust**（axum + tokio + tokio-tungstenite + rustls；`x86_64-unknown-linux-musl` 静态编译单二进制，零运行时依赖） |
| 跨局域网文件可达性 | 所有设备都有**公网可达地址**，文件对端 `ext_ip:port` 直取，服务端纯通知 |
| 信任 / 组网模型 | 服务端用 **Network**（`Network ID / Token / Name / Description / Nodes / Created`）；客户端输 Token 连接后处于**未启用**状态，**需管理员登录服务端确认启用**才完成双向信任；「禁用」既用于初始审批也用于后续**临时停用**，且仅关闭服务端中转的跨 LAN 路径，不影响局域网直连 |
| 同局域网判定 | **两者结合**：默认按本机子网推断，允许手动覆盖网络名（`lanGroup`） |
| 管理页面 | 服务端内置**管理页面（Web）**，需**登录**后使用 |
| 存储 | **文件存储**（JSON 文件），不使用数据库 |

## 3. 总体架构

```
        ┌──────────────────────── ClipSync 服务端（Rust 单二进制） ────────────────────────┐
        │  · Network 注册表(文件) · 在线状态 · 文字中继(密文) · 文件通知(仅元数据)           │
        │  · 管理页面 /api/admin/*（登录后） · 设备启用审批（pending → enabled）             │
        └───────▲───────────────────────────────┬───────────────────────────────▲─────────┘
      WS 常连(设备) │                                                │  HTTPS 管理页面(管理员)
        ┌─────────┴────┐                                    ┌──────────────────────┴──────────┐
        │ 局域网 A      │                                    │ 管理员浏览器（登录后审批设备）  │
        │  A1 ── A2     │                                    └───────────────────────────────────┘
        └──────────────┘
   设备填 Token 连接 → 服务端记为「未启用(pending)」→ 管理员登录确认「启用」→ 双向信任建立
```

- 所有设备常连服务端一条 WS（Token 鉴权 + 心跳）。
- 设备连上仅代表「知道网络密钥」，**不代表已信任**；须管理员在管理页面「启用」后才进入活跃节点、参与同步。
- 同 `lanGroup` 设备：客户端自行走 mDNS 直连，**直连 WS 不经过服务端**，服务端无权干预。
- 跨 `lanGroup` 设备（且均已启用）：文字走服务端中继；文件走对端对外 `ip:port` 直取。
- **启用 / 禁用 仅作用于服务端中继门控（跨 LAN 路径）**：禁用某设备只让它不再参与服务端中转，其同 LAN 直连完全不受影响。

## 4. 安全模型

- **Network Token = 共享密钥**（设备侧）。客户端用 `HKDF(Token, salt=NetworkID)` 派生 AES-256-GCM 密钥，用于文字中继端到端加密 + 文件拉取签名。服务端只转发密文。
- **服务端不存明文 Token**：Token 在创建网络时生成一次并返回，服务端仅存其哈希（SHA-256）用于连接校验；轮换后旧 Token 失效。
- **两步信任（核心）**：
  1. 设备持 Token 连上 → 服务端校验 Token 正确 → 注册为 `enabled=false`（pending），**不**纳入活跃节点、不中继。
  2. 管理员登录管理页 → 对 pending 设备「启用」→ `enabled=true` → 服务端发 `activated` 事件 + 广播 `nodes_update` → 双向信任完成。
  3. 可随时「禁用」→ 回到 `enabled=false`（等于 pending 的中继门控），服务端发 `deactivated` 并推 `nodes_update`（移除该节点）。**禁用是运行时临时停用**：该设备仍与服务端保持 WS 连接（`online=true`），但服务端不再向其转发、也不再转发其发出的跨 LAN 流量；其同 LAN mDNS 直连不受影响（直连不经过服务端）。
- **管理员登录（独立凭证）**：与服务端管理的「操作员」账号区分于设备 Token。账号存 `data/admin.json`（用户名 + argon2 哈希）；会话用服务端密钥签名的 httpOnly Cookie / JWT。管理 API 必须登录。
- **传输安全**：服务端前置 nginx 终止 TLS（复用你现有 `api.dumengzhao.cn` 环境），WS 与 admin 页面均走 HTTPS/WSS。

### 4.1 禁用语义详解（临时停用 = 仅关断服务端跨 LAN 中继）

「禁用」复用 `enabled=false` 状态，既用于初始审批（未启用 pending），也用于上线后的临时停用。语义边界：

- 服务端**只**中转跨 `lanGroup` 的流量；同 `lanGroup` 走客户端 mDNS 直连，直连 WS 不经过服务端，服务端也无权干预。
- 因此「禁用某设备」= 服务端停止向该设备转发、也停止转发该设备发出的跨 LAN 文字 / 文件通知。**它不改变该设备与同 LAN 设备的直连。**

**示例（你的需求）**：A、B 在局域网 1（同 `lanGroup=g1`），C、D 在局域网 2（同 `lanGroup=g2`），全部接入服务端。若管理员禁用 B：

| 路径 | 结果 | 说明 |
|---|---|---|
| B 复制 → C、D | ❌ CD 无法发现 B 的复制 | 服务端不向 g2 中继（B `enabled=false`） |
| B ↔ A | ✅ 正常同步 | mDNS 直连，不经过服务端，不受禁用影响 |
| C 复制 → B | ❌ B 收不到 CD 跨 LAN 复制 | 服务端不向已禁用 B 转发 |
| C 复制 → A | ✅ A 正常收到 | 同 g2→g1 的中继，A 仍启用 |

即：**禁用只让设备从「跨 LAN 视野」下线，本地局域网信任链始终在线**。这也意味着服务端是天然的单点——它宕机只砍断跨 LAN，局域网内 A/B 照常同步。

## 5. 服务端设计（Rust）

### 5.1 文件存储（无数据库）
目录 `server/data/`（或环境变量 `CLIPSYNC_DATA_DIR`）：
- `networks.json`：`Network[]`，Token 仅存哈希；结构见 5.2。
- `admin.json`：`{ user, pass_hash }`（argon2）；首次运行若缺失则按 `ADMIN_USER`/`ADMIN_PASS` 环境变量生成，或控制台打印随机密码。
- `server.key`：服务端会话签名密钥（缺失则随机生成并落盘）。
- 写入策略：内存为权威缓存，变更时**原子写**（写临时文件 + rename）落盘，重启从文件恢复。

### 5.2 数据模型
```rust
struct Network {
    id: String,
    token_hash: String,   // SHA-256(Token)，不存明文
    name: String,
    description: String,
    nodes: Vec<Node>,
    created: i64,
}
struct Node {
    device_id: String,
    name: String,
    lan_group: String,
    ext_file_ep: String,  // 公网 ip:port
    platform: String,
    enabled: bool,        // false = 未启用(pending)，true = 已启用(双向信任)
    online: bool,
    last_seen: i64,
}
```

### 5.3 设备 WS 连接与消息（JSON）
Client → Server:
- `auth { token, device:{ id, name, lan_group, ext_file_ep, platform } }`
- `heartbeat`
- `relay_text { to, ct }`     // ct = base64 密文（仅已启用设备可发/收）
- `file_notify { manifest, ext_file_ep }`

Server → Client:
- `welcome { status: "pending"|"active", network:{ id, name }, nodes:[已启用节点] }`
- `activated`                // 管理员启用后下发，此后可参与同步
- `deactivated`              // 被禁用后下发
- `nodes_update { nodes:[已启用节点] }`
- `relay_text { from, ct }`
- `file_notify { from, manifest, ext_file_ep }`
- `error { code, msg }`

### 5.4 设备连接行为（含两步信任）
- `auth`：服务端对 Token 取哈希查 Network：
  - 查到 → 注册/更新 Node（`enabled` 保持原值；首次为 `false`）。回 `welcome`，`status` 反映当前启用态，`nodes` 仅含**已启用**节点。
  - 查不到 → `error { code:"bad_token" }` 并关闭。
- 心跳超时（如 30s）→ `online=false`，向已启用节点推 `nodes_update`。
- `relay_text` / `file_notify`：**仅当本机与该目标 Node 均 `enabled=true` 才转发**；`enabled=false`（pending 或已禁用）设备发来的中继直接忽略/报错。禁用即与服务端 pending 等价——该设备对所有跨 LAN 设备不可见、不可被中继；但其同 LAN 直连不受影响（直连不经过服务端，见 §4.1）。
- 管理员「启用」某设备 → `enabled=true`，向其 WS 发 `activated`，并向本网络已启用节点推 `nodes_update`（含新节点）。
- 管理员「禁用」→ `enabled=false`，发 `deactivated`，推 `nodes_update`（移除该节点）。

### 5.5 管理页面 + 登录（Admin Web）
- 服务端用 axum 同时提供：
  - 设备 WS 端点（如 `/ws`）。
  - 管理 REST API（`/api/admin/*`，需登录）。
  - 静态管理页面（打包进二进制：`rust-embed` / `include_dir`，零额外部署）。
- 登录：
  - `POST /api/admin/login { user, pass }` → 校验 `admin.json` → 下发签名 httpOnly Cookie（或 JWT）。
  - 管理 API 用中间件校验会话。
- 管理 API：
  - `GET  /api/admin/networks` → 网络列表（含 pending/active 设备数）。
  - `POST /api/admin/networks` → 创建网络，返回 **Token 一次**（仅此次明文）。
  - `POST /api/admin/networks/:id/reset-token` → 轮换 Token（返回新 Token；所有客户端需更新）。
  - `GET  /api/admin/networks/:id/devices` → 设备列表（含 pending 与 active）。
  - `POST /api/admin/networks/:id/devices/:deviceId/enable` → 启用（触发 `activated`）。
  - `POST /api/admin/networks/:id/devices/:deviceId/disable` → 禁用（触发 `deactivated`）。
  - `DELETE /api/admin/networks/:id` → 删除网络（可选）。
- 管理页面（纯 HTML/JS，免构建，保持单二进制）：网络列表、待审批(pending)设备、活跃设备、启用/禁用按钮、Token 显示与重置。

### 5.6 构建与部署
- `cargo new` 于 `server/`；依赖：`tokio`、`axum`、`tokio-tungstenite`、`rustls`（避免 openssl 系统依赖）、`argon2`、`sha2`、`serde`、`rust-embed`。
- 部署：本机 `cargo build --release --target x86_64-unknown-linux-musl` → 单二进制 `scp` 到 VPS，`nginx` 反代 `/ws` 与 `/api/admin`、`/admin`；`systemd` 守护。
- 可选 `Dockerfile`（rust:alpine 多阶段构建 musl 静态二进制）+ `docker-compose.yml`，一条命令起。

## 6. 客户端改造（Rust / Tauri）

### 6.1 新增 ServerConn + 启用态
- 独立 WS 连接服务端，持 Token 鉴权；断线指数退避重连。
- 收到 `welcome.status`：
  - `pending` → UI 显示「已连接，等待服务端启用」，设备暂不可用于同步。
  - `active` → 进入正常同步（合并节点表）。
- 收到 `activated` → 切到 active，开始参与同步；收到 `deactivated` → 回 pending，停同步。

### 6.2 路由决策（核心，仅对 active 节点）
本机剪贴板变化（文字或文件 manifest）时：
- 对「同 `lanGroup` 且已直连」的节点 → 走现有直连 WS（不变）。
- 对「跨 `lanGroup` 且均已启用」的节点 →
  - 文字：网络密钥加密 → `relay_text` 发服务端 → 服务端转发对端 → 对端解密写剪贴板。
  - 文件：发 `file_notify`（仅 manifest + 本机 `extFileEp`）→ 服务端广播 → 对端 UI 显示「待复制」。

### 6.3 lanGroup 判定（两者结合，不变）
- 默认：本机主网卡 IPv4 前 24 位哈希。
- 配置项 `lanGroup`（手动覆盖）优先。
- 两设备「同 LAN」⇔ `lanGroup` 相同。

### 6.4 内嵌 HTTP 文件服务（新组件，不变）
- 客户端启动小 HTTP server，监听对外 `extFileEp`（公网 `ip:port`）。
- `GET /file/<hash>` → 流式返回已复制并共享的文件字节（仅本网络成员可拉，Token 签名校验）；建议 TLS 或至少签名。
- 跨 LAN 拉取：收到 `file_notify` → UI 显示待复制 → 点「拉取」→ HTTP GET 对端 `extFileEp/file/<hash>` → 落盘 → 写本地剪贴板为文件路径（复用 `apply_remote`）。

### 6.5 UI 调整
- 设备列表合并两类来源：局域网直连（mDNS）+ 服务端跨 LAN（已启用节点），标注来源。
- 新增设备「连接状态」：未启用(pending) / 已启用(active)。
- 新增「待复制（跨 LAN）」列表。
- 设置页新增：服务端地址、Network Token、对外 `ip:port`、`lanGroup`（可选）。
- 局域网发现「刷新」按钮保留（仅刷 LAN）。

## 7. 实施路线图（建议分阶段，每阶段可独立验证）

1. **服务端骨架（Rust + 文件存储）** ✅ 已完成并冒烟通过：axum + WS；`networks.json`/`admin.json`/`server.key` 读写；Network 模型 + Token 哈希；`auth`/`heartbeat`；`enabled` 门控的中继 + `nodes_update`。（裸 ws 客户端手测 pending→active 全通过）
2. **管理页面 + 登录** ✅ 已完成：管理员登录（`POST /api/admin/login` 校验密码后用 `server_key` 签发 HMAC-SHA256 会话 token，7 天有效）；`admin_auth` 中间件统一校验；`rust-embed` 打包 `static/admin.html` 进二进制（`GET /admin`）；网络/设备审批/启用禁用/Token 重置。已联调：未登录拦截、错误密码拦截、登录拿 token、授权访问、创建网络、页面返回、伪造 token 拒绝。
3. **客户端 ServerConn + pending/activated 流程**：连接、鉴权、接收 `welcome` 状态、合并已启用节点（先只读展示）。
4. **文字跨 LAN 中继**：路由决策 + 网络密钥加解密 + `relay_text` 端到端。
5. **文件跨 LAN**：内嵌 HTTP 文件服务 + `file_notify` + 待复制列表 + 拉取落盘。
6. **UI / 设置收尾**：服务端 / Token / `extFileEp` / `lanGroup` / 连接状态配置项与展示。
7. **测试与加固**：两局域网各一台联调；pending→启用全流程；Token 轮换；启用/禁用；nginx TLS；心跳/重连。

## 8. 风险与待定

- 公网可达性依赖用户侧路由/端口转发（已确认具备）。
- 服务端为单点：宕机仅影响跨 LAN；局域网直连不受影响。
- **两步信任的取舍**：pending 设备连上但不参与同步，管理员必须登录审批；若管理员不审批则设备永远不可用（符合「需确认才信任」诉求）。
- Token 兼作密钥：泄露即该网络可读；轮换需所有客户端更新（用户已知）。
- 管理员账号为服务端最高权限，需强密码；`admin.json` 妥善保管。
- 文件直取建议 TLS，否则明文过公网（至少签名校验）。
- 服务端是否持久化「最近一条文字」给晚到节点：暂定不持久（纯实时），如需可加。
- 部署位置 / 域名：建议与你现有 `api.dumengzhao.cn` 同机或子域，复用 nginx 反代。
- 客户端与服务端如共享 `crypto`（Token→密钥派生），建议抽成 `clipsync-crypto` crate 共用，避免两端算法漂移；否则在两端各实现一份并固定参数。
