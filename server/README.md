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

> **从 Windows 主机交叉编译** 还需两步（纯 Rust，无需安装任何 Linux C 工具链）：
> 1. 复制 Rust 自带的 ELF 链接器 `ld.lld` 到 cargo bin：
>    `cp "$HOME/.rustup/toolchains/stable-x86_64-pc-windows-msvc/lib/rustlib/x86_64-pc-windows-msvc/bin/rust-lld.exe" "$HOME/.cargo/bin/ld.lld.exe"`
> 2. 构建时开启 `RUSTC_BOOTSTRAP=1`（`.cargo/config.toml` 已为该目标配好 `linker-flavor=gnu-lld` + `link-self-contained=yes`，用自带 musl libc 启动对象与 rust-lld）。
>
> 产出为 **全静态 x86_64 ELF**，可直接拷到任意 glibc / musl 的 Linux 主机运行，零运行时依赖。
> 注：服务端会话 JWT 已改为纯 Rust 自实现 HS256（移除 `jsonwebtoken`/`ring`），故整个依赖图纯 Rust，musl 目标无需 C 编译器。

前置 nginx 终止 TLS 并反代 `/ws`、`/api/admin`、`/admin`。可选 `Dockerfile` + `docker-compose.yml`。

## Linux 部署（systemd 常驻）

二进制是全静态 ELF、零运行时依赖，适合直接跑在 Ubuntu 等主机上当常驻中继。

> **打包成单目录**（本机 Windows 交叉编译后）：`bash package.sh` 自动把二进制 + `install.sh` + `clipsync-server.service` + `clipsync-server.env.example` + `nginx-clipsync.conf.example` 汇总到 `server/dist/clipsync-server-linux/`。把该目录**整目录**拷到服务器，进去 `sudo ./install.sh` 即可，不用管文件对应关系。

> **一键部署**：在 Linux 主机 `git pull` 拿到脚本与二进制后，一条命令搞定（建用户/目录、装二进制 + service + env、随机生成管理员密码、启用并启动）：
> ```bash
> sudo ./install.sh            # 自动找脚本同目录 / target 下的 clipsync-server
> # 或显式指定二进制： sudo ./install.sh /path/to/clipsync-server
> # 只安装不启动：     sudo ./install.sh --no-start
> ```
> 脚本会自动回显随机管理员密码，起好后 `curl 127.0.0.1:20070/healthz` 应返回 ok。下面为分步说明（脚本就是从这里来的）。
>
> **只拷文件到服务器（非 git pull）也行**：把 `install.sh` 和编译好的 `clipsync-server` 放在**同一目录**，`sudo ./install.sh` 即可——`.service` / `.env.example` 脚本会**自动生成**，不必一起拷；`/opt/clipsync-server`、`/etc/systemd/system/` 是脚本自动创建的**目标**位置，你不用预先建。也可 `sudo ./install.sh /绝对路径/clipsync-server` 显式指定二进制。

1. **取得二进制**：本机交叉编译后把 `clipsync-server` 拷到 Linux；或在 Linux 主机 `rustup target add x86_64-unknown-linux-musl && cargo build --release --target x86_64-unknown-linux-musl`。
2. **放文件**（以 `/opt/clipsync-server` 为例）：
   - `clipsync-server` → `/opt/clipsync-server/clipsync-server`（`chmod +x`，属主 `root`）
   - `clipsync-server.env.example` 复制为 `/opt/clipsync-server/clipsync-server.env` 并改密码
   - `clipsync-server.service` → `/etc/systemd/system/`
   - 建用户/目录：`useradd -r -d /opt/clipsync-server -s /usr/sbin/nologin clipsync && mkdir -p /opt/clipsync-server/data && chown -R clipsync:clipsync /opt/clipsync-server`
3. **起服务**：
   ```bash
   systemctl daemon-reload
   systemctl enable --now clipsync-server
   systemctl status clipsync-server   # 看 /healthz 是否 ok
   ```
4. **nginx 反代**（终止 TLS）：`/ws`、`/api/admin`、`/admin` 转发到 `127.0.0.1:20070`。具体配置见 **`nginx-clipsync.conf.example`**（已含 `/ws` 的 `Upgrade`/`Connection` 头映射与 3600s 长超时、`/healthz` 探针、80→443 跳转）。改好 `server_name`/证书路径后：`nginx -t && systemctl reload nginx`。
5. **看日志 / 健康检查**：
   ```bash
   journalctl -u clipsync-server -f          # 实时跟踪服务日志
   curl -s 127.0.0.1:20070/healthz          # 期望 ok（未走 nginx 时）
   curl -s https://你的域名/healthz         # 走 nginx TLS 时
   ```

> **走 nginx TLS 时，桌面端「服务器地址」填 `wss://域名`（端口 443），不要再填 `ip:20070`**，否则客户端会直连未加密的 20070 端口、绕过 TLS。
> 数据落在 `CLIPSYNC_DATA_DIR`（默认 `data/`），含 `networks.json` / `admin.json` / `server.key`；迁移机器时连同该目录一起拷即可。

## 状态

方案 v2 已确认（Rust / 管理页+登录 / 文件存储 / 两步信任启用）。分阶段实现中，见架构方案第 7 节路线图。

### 阶段 1（已完成 ✅）：服务端骨架 + 文件存储 + 设备 WS + 中继门控

- 设备 WS `/ws`：`auth`(Token 哈希校验) → 注册/更新节点（首次 `enabled=false`/pending）→ `welcome`；`heartbeat`；`relay_text`/`file_notify` 仅当源与目标均 `enabled=true` 且跨 `lan_group` 才转发（密文透传 / 仅广播 manifest）。
- 文件存储：`data/networks.json`(Token 仅存哈希) / `data/admin.json`(argon2) / `data/server.key`，原子写 + 启动恢复。
- 联调脚本已验证：pending→启用→跨 LAN 中继→禁用停发 全部通过。

### 阶段 2（已完成 ✅）：管理页面 + 登录会话

- **登录会话** 取代临时 `ADMIN_API_KEY`：`POST /api/admin/login` 校验账号密码后，用 `server.key` 做 HMAC-SHA256 签发会话 token（7 天有效）；管理 REST 与页面统一由 `admin_auth` 中间件校验该会话。
- **内嵌管理页**（`rust-embed` 编译时打包进二进制，免部署静态文件）：`GET /admin` 返回 `static/admin.html`（登录、网络列表、创建网络返回一次性 Token、设备列表及启用/禁用）。
- 阶段2 联调脚本已验证：未登录拦截(401)、错误密码拦截、登录拿 token、授权访问、创建网络、`/admin` 返回页面、伪造 token 被拒。

### 本地运行

```bash
cd server
cargo build                 # 或 cargo build --release
CLIPSYNC_DATA_DIR=./data \
ADMIN_USER=admin \
ADMIN_PASS=你的密码 \
LISTEN=0.0.0.0:20070 \
./target/debug/clipsync-server
```

- 打开管理后台：`http://host:20070/admin`（首次用 `ADMIN_USER`/`ADMIN_PASS` 登录）。
- 健康检查：`GET /healthz`
- 创网络（拿 Token）：登录后在前端「创建网络」按钮，或
  `curl -X POST -H "Authorization: Bearer <session>" -d '{"name":"默认","description":""}' http://127.0.0.1:20070/api/admin/networks`
  其中 `<session>` 由 `POST /api/admin/login` 返回。
- 设备接入：`ws://host:20070/ws`，首条消息 `{"type":"auth","token":"<Token>","device":{"id":"...","name":"...","lan_group":"g1","ext_file_ep":"ip:port","platform":"mac"}}`
