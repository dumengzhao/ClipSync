# ClipSync 客户端更新模块方案（中继服务内置 · 自建托管 · Option A · 无签名）

> 状态：**已实现**（2026-09-02，按第 12 节顺序分步落地：服务端 update.rs +
> 管理上传 + 管理页 UI + 移除 Tauri updater 插件 + 客户端自写更新器 +
> publish-update.sh）。目标：把客户端自动更新与中继服务合建在同一进程/同一台机器上，
> 通过管理端上传「自定义 `latest.json` + 安装包」，客户端用**用户自己配置的中转服务地址**
> 拉取并安装更新。
>
> **本次修订核心决策（2026-09-02，用户拍板）**：
> 1. **更新不做签名**。信任锚 = 用户自己的中继服务器 + TLS，不做 ed25519 签名校验。
> 2. **更新地址来自用户配置的 relay 地址**（同步里已填的地址），不硬编码作者公网服务器、不指向 GitHub。
> 3. **不留可选签名后门**，manifest 结构彻底不含 pubkey/signature，保持最简。
> 4. 因 Tauri 内置 `updater` 插件**强制签名、无法关闭**，客户端**不能**用它，必须自写轻量更新器。
>    移除 `tauri.conf.json` 的 `updater` 插件后，`tauri build` 也不再要求 `TAURI_SIGNING_PRIVATE_KEY`（解决此前打包卡点）。

---

## 1. 目标与范围

- **托管方式**：自建，复用现有腾讯云中继服务（`root@152.136.229.188`，axum + rustls/可走 nginx TLS）。开源后别人各部署各的中继+更新。
- **发布方式（已拍板 = Option A）**：管理端上传 API + 最小管理页表单，浏览器传包，无需 SSH。
- **信任模型（自托管）**：操作者完全信任自己的服务器与 TLS 证书。服务器/TLS 被攻破即可对该服务器名下客户端推更新——由操作者自担，合理。**绝不**把更新 URL 硬编码成作者服务器，避免陌生人用作者二进制=全信作者。
- **客户端**：自写更新器（Rust 命令 + 前端按钮），**不用** Tauri `updater` 插件。
- **不在本次范围**：GitHub Release 双源、增量/差分更新、强制更新后端化、签名体系。

## 2. 总体架构

```
浏览器(admin) ──POST /api/admin/update──┐  (JWT, admin_auth)
                                        ▼
                中继服务 axum  ┌──────────────────────────┐
                (20070)       │  update 模块 (新增)        │
                              │  UPDATE_DIR/               │
                              │    latest.json  (自定义,无签名)
                              │    files/<platform>/*.exe │
                              └──────────────────────────┘
                                        │  GET /update/latest.json (公开读, url 自动改写)
                                        │  GET /update/files/:p/:f (公开读)
                                        ▼
                  客户端 (自写更新器) 用「用户配置的 relay 地址」拼 /update/latest.json
                  → 比对版本 → 下载对应平台安装包 → 校验 sha256 → 运行安装包
```

- 服务端只做静态托管 + 管理上传，**不验证安装包真伪**（无签名）。信任由「服务器 + TLS」承担。
- `latest.json` 的 `url` 由服务端**返回时**按自身 origin 改写，发布端无需手改。
- 下载后客户端用 manifest 里的 `sha256` 校验**完整性**（不证明来源真伪，但挡传输损坏/缓存篡改）。

## 3. 配置（环境变量）

在现有 `clipsync-server.env.example` 增加三项（均可选，有默认值）：

| 变量 | 默认 | 说明 |
|------|------|------|
| `UPDATE_DIR` | `<CLIPSYNC_DATA_DIR>/update` | 更新文件根目录 |
| `UPDATE_PUBLIC_BASE` | 空（回退请求 Host） | 改写 `latest.json` 用的公开基址，如 `https://sync.example.com` |
| `UPDATE_MAX_UPLOAD_MB` | `200` | 单次上传总大小上限（防滥用） |

目录结构：
```
<UPDATE_DIR>/
  latest.json                      # 自定义清单（服务端返回时改写 url）
  files/
    windows-x86_64/   <安装包>.exe (.nsis)
    windows-aarch64/  ...
    darwin-x86_64/    ...dmg / .app
    darwin-aarch64/   ...
    linux-x86_64/     ...AppImage / .deb
```

## 4. 自定义 `latest.json` 结构（无签名）

```json
{
  "version": "0.1.1",
  "notes": "修复托盘设置最小化后点不开",
  "pub_date": "2026-09-02T00:00:00Z",
  "platforms": {
    "windows-x86_64": {
      "url": "https://sync.example.com/update/files/windows-x86_64/ClipSync_0.1.1_x64-setup.exe",
      "sha256": "a1b2c3..."
    },
    "darwin-aarch64": { "url": "...", "sha256": "..." },
    "linux-x86_64":   { "url": "...", "sha256": "..." }
  }
}
```
- **无 `signature`、无 `pubkey`** 字段（本次决策第 3 点）。
- 校验只用 `sha256`（完整性，来源真伪由 TLS + 服务器保证）。

## 5. 服务端模块设计（`server/src/update.rs`，新增）

### 5.1 路由（在 `main.rs::build_router` 中挂载）

公开（无鉴权，检查更新用，不带设备 token）：
- `GET /update/latest.json` → 读 `<UPDATE_DIR>/latest.json`，`application/json` 返回；不存在返回 404。
- `GET /update/files/:platform/:file` → 读文件字节，`application/octet-stream` + `Content-Disposition: attachment`；支持 `Range`（可选，先做到整文件返回）。

管理（挂在现有 `admin_auth` 中间件下，复用 `admin::admin_auth`）：
- `GET /api/admin/update` → 返回当前线上版本摘要（解析 `latest.json` 的 `version`/`pub_date`/`notes`，缺则 404）。
- `POST /api/admin/update` → multipart 上传：字段 `manifest`(`latest.json`) + 一个或多个 `file`(带 `platform`/`filename` 表单字段)，落盘到 `UPDATE_DIR`。

### 5.2 依赖变更

- 加入 multipart 支持：axum 的 `axum::extract::Multipart`（开启 axum 的 `multipart` feature）。
- 若需流式发文件：引入 `tokio-util` 的 `ReaderStream`（当前 `Cargo.toml` 未见，需加）。
- 其余复用现有（`axum`、`tokio`、`serde_json`、`rust-embed`）。

### 5.3 `latest.json` 返回时的 url 改写算法

- 解析为 serde 结构；遍历 `platforms`，对每个 `url` 取 `Path::basename`，重写为
  `<base>/update/files/<platform>/<basename>`。
- `<base>` 优先用 `UPDATE_PUBLIC_BASE`；为空则用请求的 `Origin`/`Host`（**仅当为 https 时**，否则报错，避免生成 http 链接——TLS 是无签名模型的唯一安全边界）。
- 改写后序列化返回。管理端只传原始 `latest.json`+文件，无需关心部署域名。

### 5.4 上传处理（POST /api/admin/update）

1. `admin_auth` 已保证是管理员。
2. 解析 multipart：
   - `manifest` 字段 → 校验能解析为自定义 schema（至少含 `version` + `platforms`，每平台有 `url` 与 `sha256`）→ 临时写 `latest.json.tmp` → rename。
   - `file` 字段 → 必须带 `platform`（须匹配已知键集合白名单：`windows-x86_64`/`windows-aarch64`/`darwin-x86_64`/`darwin-aarch64`/`linux-x86_64` 等）与 `filename` → 写 `<UPDATE_DIR>/files/<platform>/<filename>`（先写 tmp 再 rename）。
3. 安全：
   - `filename` 必须不含 `/`、`\`、`..`，否则拒绝（防目录穿越）。
   - 单文件大小 + 累计大小受 `UPDATE_MAX_UPLOAD_MB` 限制（流式计数，超限中断）。
   - 仅允许 `POST`，且必须带 admin JWT。
4. 成功后返回 `{ok:true, version}`。

### 5.5 存储习惯

沿用 `storage.rs` 既有套路：`create_dir_all` 建目录 → 写 `*.tmp` → `rename` 原子替换，避免半截文件被客户端拉到。

## 6. 客户端自写更新器设计（替代 Tauri updater 插件）

> 因 Tauri `updater` 插件强制签名无法关闭，必须自写。这是与旧方案最大的区别。

### 6.1 更新地址来源
- 复用同步配置里**用户已填的 relay 地址**（如 `wss://host:port/ws` 或 `https://host:port`）。
- 取该地址的 https origin 作为更新基址：`https://<host>`（端口按需保留），拼 `/update/latest.json`。
- **绝不硬编码作者服务器域名**（决策第 2 点安全底线）。

### 6.2 Rust 命令（`client/src-tauri/src/update.rs`，新增）
- `check_update() -> Option<UpdateInfo>`：GET `<base>/update/latest.json`（公开，无 token）→ 解析 → 与 `env!("CARGO_PKG_VERSION")`/当前版本比对 → 返回是否有更新 + notes + 本平台下载 url + sha256。
- `download_update(url, sha256) -> PathBuf`：下载安装包到临时目录 → 计算 sha256 比对 manifest → 不一致则删掉报错 → 返回路径。
- `install_update(path)`：按平台拉起安装包：
  - Windows：执行 nsis `.exe`（被动 `/S` 或 `installMode` 对应静默），随后退出当前进程由新版本接管。
  - macOS：打开 `.dmg`/`.app`（或 `open` 引导用户拖移）。
  - Linux：执行 `.AppImage`/调 `dpkg -i`。

### 6.3 前端（设置页）
- 「检查更新」按钮 → 调 `check_update` → 展示「已是最新 / 发现 vX.Y.Z（notes）」＋「下载并安装」按钮。
- 可选：启动时静默 `check_update`，有更新仅显示小红点/角标，不自动下载（贴合极简 UI 偏好）。

## 7. 管理页（最小 UI）

现有 admin 页由 `rust-embed` 内嵌（`admin.rs` 的 `#[derive(RustEmbed)]` + `/admin`、`/admin/static/:p`）。

- 新增一个"客户端更新"区域：
  - 顶部 `GET /api/admin/update` 拉当前线上版本并展示（版本号 / 发布时间 / 各平台是否已上传）。
  - 一个 multipart 表单：1 个 `latest.json` 文件选择 + 多平台安装包文件选择（每个标注 platform），提交到 `POST /api/admin/update`。
- 实现方式二选一（实现时定）：
  - (a) 在现有内嵌 admin HTML 里加一段 section；
  - (b) 新增内嵌页 `admin_update.html` + 路由 `GET /admin/update` 单独展示。
- 不做复杂进度条，成功/失败用文字提示即可。

## 8. 配套改动清单

| 文件 | 改动 |
|------|------|
| `server/src/update.rs` | 新增模块（路由 + 改写 + 上传，无签名） |
| `server/src/main.rs` | `build_router` 挂载 `/update/*` 与 `/api/admin/update`，注入 `UPDATE_DIR` 等到 `AppState` |
| `server/src/admin.rs` | 提供 `admin_auth` 复用；新增管理页片段/路由 |
| `server/src/state.rs` / `AppState` | 增加 `update_dir: PathBuf`、`update_public_base: Option<String>`、`update_max_upload: u64` |
| `server/clipsync-server.env.example` | 增加 `UPDATE_DIR` / `UPDATE_PUBLIC_BASE` / `UPDATE_MAX_UPLOAD_MB` 三项与注释 |
| `server/Cargo.toml` | 加 multipart / 可能 `tokio-util` 依赖 |
| `server/nginx-clipsync.conf.example` | 增加 `location /update/` 反代到中继 20070（TLS 在此终止，是信任边界） |
| `client/src-tauri/tauri.conf.json` | **移除整个 `plugins.updater` 块**（不再用 Tauri updater 插件） |
| `client/src-tauri/src/update.rs` | 新增：自写 `check_update` / `download_update` / `install_update` |
| `client/src`（前端） | 设置页「检查更新」入口 + 可选启动静默检查 |
| `server/publish-update.sh`（新增） | 本机 `tauri build` → 生成自定义 `latest.json`（含各平台 sha256）→ 调 admin 上传 API 推上服务器 |

## 9. nginx 片段（示例，加入 `nginx-clipsync.conf.example`）

```nginx
location /update/ {
    proxy_pass http://127.0.0.1:20070;
    proxy_set_header Host $host;
    proxy_set_header X-Real-IP $remote_addr;
    client_max_body_size 256m;   # 允许上传大安装包
}
```
> TLS 在 nginx 终止，中继 20070 走内网。`/update/` 全走 https 是**无签名模型唯一的安全边界**——务必启用且证书校验不可省。

## 10. 客户端构建前置（不再需要签名）

移除 Tauri `updater` 插件后，`tauri build` **不再要求 `TAURI_SIGNING_PRIVATE_KEY`**，打包卡点消失。

1. Windows 装 **NSIS**（msi 还需 **WiX**）或其他平台对应打包工具，让 `tauri build` 能出安装包。
   - 当前本机有 NSIS，可出 `ClipSync_x.y.z_x64-setup.exe`；WiX 缺失则暂无 msi（不影响更新逻辑，只是少一种安装包）。
2. `tauri build` 产出 `target/release/bundle/**` 下安装包；**`latest.json` 改为我们自己生成**（见下）。
3. `publish-update.sh`：
   - 调 `tauri build`；
   - 对每个产物算 `sha256sum`；
   - 生成自定义 `latest.json`（version 取 `Cargo.toml` 的 version + 各平台 url + sha256 + notes）；
   - 通过 admin API 把 `latest.json`+安装包推上服务器。
4. 若暂不装打包工具链，可先用"占位 latest.json + 一个小文件"验证服务端收发链路，再补真实构建。

## 11. 验证计划

1. **服务端（cargo test）**：
   - url 改写（各 platform 正确、basename 提取、base 回退仅 https）。
   - 目录穿越：`/update/files/../secret` 返回 404/拒绝。
   - 上传：`curl -F` 模拟 admin 上传 `latest.json`+文件 → `GET /update/latest.json` 返回改写后 url → `GET /update/files/...` 能下载。
   - 未授权：`POST /api/admin/update` 无 JWT 返回 401。
2. **链路冒烟（本机）**：console 模式起 relay，`curl` 验证两个公开端点；admin 页表单上传后版本号正确显示。
3. **客户端**：管理端上传真实构建产物后，设置页「检查更新」能拿到新版本；下载后 **sha256 比对一致** 才安装；Windows 用 nsis 被动模式安装并重启。
4. **回归**：`cargo build` / `cargo test` 全绿；中继既有 `/ws`、`/healthz`、`/admin` 不受影响；`tauri.conf.json` 无 `updater` 插件后 `tauri build` 不再报缺密钥。

## 12. 实施顺序（建议提交粒度）

1. 服务端 `update.rs` + 路由挂载 + `AppState` 配置项 + `env.example`（纯收发，先不起管理页）。
2. 服务端管理上传 API + 安全校验（multipart、白名单、大小限制、原子写）。
3. 管理页 UI（版本展示 + 上传表单）。
4. nginx 片段 + **客户端移除 `updater` 插件**。
5. 客户端 `update.rs` 自写更新器（check/download/install）+ 前端「检查更新」入口 + 更新地址取自 relay 配置。
6. `publish-update.sh` + 构建前置说明（仅打包工具链，无签名）。
7. 全量自测 + 提交推送（每步验证后 `commit`+`push`，对齐双机联调习惯）。

---

## 待确认/风险

- **TLS 是无签名模型的唯一安全边界**：`/update/` 必须 https 且证书校验不可省；`UPDATE_PUBLIC_BASE` 缺失时若请求非 https origin，改写应报错 500（生产务必配 `UPDATE_PUBLIC_BASE`）。
- **更新 URL 必须来自用户 relay 配置**：绝不能硬编码作者服务器，否则陌生人用作者二进制=全信作者服务器（决策第 2 点安全底线）。
- **无签名 = 服务器/TLS 被攻破即 RCE**：自托管模型下由操作者自担；如需硬核防护，将来可单独加回签名（本次明确不做）。
- **大文件上传**：msi/nsis 可能 >100MB，nginx `client_max_body_size` 与 `UPDATE_MAX_UPLOAD_MB` 需对齐，且 axum 侧建议流式计数避免 OOM。
- **多平台发布频率**：管理端一次可传多平台；若只传部分平台，`latest.json` 仍声明全部，缺失平台客户端下载会 404——发布脚本须保证"传齐再换 latest.json"（原子替换已部分覆盖此风险）。
- **管理页实现方式**：内嵌现有页加段 vs 新独立页（实现时选，不影响接口）。
