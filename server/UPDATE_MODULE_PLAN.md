# ClipSync 客户端更新模块方案（中继服务内置 · 自建托管 · Option A）

> 状态：计划文档（未实现）。目标：把客户端自动更新与中继服务合建在同一进程/同一台公网机器上，
> 通过管理端上传 `latest.json` + 安装包，客户端经 `https://<域名>/update/latest.json` 拉取。
> 客户端签名校验沿用 `tauri.conf.json` 现有 `pubkey`，服务端只做"存 + 发"，不碰密钥。

---

## 1. 目标与范围

- **托管方式**：自建，复用现有腾讯云中继服务（`root@152.136.229.188`，axum + rustls/可走 nginx TLS）。
- **发布方式（已拍板 = Option A）**：管理端上传 API + 最小管理页表单，浏览器传包，无需 SSH。
- **客户端**：Tauri v2 `tauri-plugin-updater`，endpoint 改指向自建 `/update/latest.json`。
- **不在本次范围**：GitHub Release 双源镜像、增量/差分更新、强制更新策略后端化（仅前端按需）。

## 2. 总体架构

```
浏览器(admin) ──POST /api/admin/update──┐  (JWT, admin_auth)
                                        ▼
                中继服务 axum  ┌──────────────────────────┐
                (20070)       │  update 模块 (新增)        │
                              │  UPDATE_DIR/               │
                              │    latest.json             │
                              │    files/<platform>/*.msi │
                              └──────────────────────────┘
                                        │  GET /update/latest.json (公开, url 自动改写)
                                        │  GET /update/files/:p/:f (公开)
                                        ▼
                              客户端 Tauri ──checkUpdate/download_and_install──▶ 安装
```

- 服务端只做静态托管 + 管理上传，不验证安装包内容（签名由客户端用 pubkey 校验）。
- `latest.json` 的 `url` 字段由服务端在**返回时**按自身 origin 改写，发布端无需手改。

## 3. 配置（环境变量）

在现有 `clipsync-server.env.example` 增加两项（均可选，有默认值）：

| 变量 | 默认 | 说明 |
|------|------|------|
| `UPDATE_DIR` | `<CLIPSYNC_DATA_DIR>/update` | 更新文件根目录 |
| `UPDATE_PUBLIC_BASE` | 空（回退请求 Host） | 改写 `latest.json` 用的公开基址，如 `https://sync.example.com` |
| `UPDATE_MAX_UPLOAD_MB` | `200` | 单次上传总大小上限（防滥用） |

目录结构：
```
<UPDATE_DIR>/
  latest.json                      # Tauri 更新清单（服务端返回时改写 url）
  files/
    windows-x86_64/   <安装包>.msi / .nsis.zip
    windows-aarch64/  ...
    darwin-x86_64/    ...app.tar.gz / .dmg
    darwin-aarch64/   ...
    linux-x86_64/     ...AppImage / .deb
```

## 4. 服务端模块设计（`server/src/update.rs`，新增）

### 4.1 路由（在 `main.rs::build_router` 中挂载）

公开（无鉴权）：
- `GET /update/latest.json` → 读 `<UPDATE_DIR>/latest.json`，`application/json` 返回；不存在返回 404。
- `GET /update/files/:platform/:file` → 读文件字节，`application/octet-stream` + `Content-Disposition: attachment`；支持 `Range`（可选，先做到整文件返回）。

管理（挂在现有 `admin_auth` 中间件下，复用 `admin::admin_auth`）：
- `GET /api/admin/update` → 返回当前线上版本摘要（解析 `latest.json` 的 `version`/`pub_date`/`notes`，缺则 404）。
- `POST /api/admin/update` → multipart 上传：字段 `manifest`(`latest.json`) + 一个或多个 `file`(带 `platform`/`filename` 表单字段)，落盘到 `UPDATE_DIR`。

### 4.2 依赖变更

- 加入 multipart 支持：axum 的 `axum::extract::Multipart`（确认需开启 axum 的 `multipart` feature / 引入 `multer`）。
- 若需流式发文件：引入 `tokio-util` 的 `ReaderStream`（当前 `Cargo.toml` 未见，需加）。
- 其余复用现有（`axum`、`tokio`、`serde_json`、`rust-embed`）。

### 4.3 `latest.json` 返回时的 url 改写算法

Tauri `latest.json` 结构（节选）：
```json
{
  "version": "1.2.3",
  "notes": "...",
  "pub_date": "2026-...",
  "platforms": {
    "windows-x86_64": { "signature": "....", "url": "https://原始地址/clipsync_1.2.3_x64_en-US.msi" }
  }
}
```
- 解析为 serde 结构；遍历 `platforms`，对每个 `url` 取 `Path::basename`，重写为
  `<base>/update/files/<platform>/<basename>`。
- `<base>` 优先用 `UPDATE_PUBLIC_BASE`；为空则用请求的 `Origin`/`Host`（仅当为 https 时，否则报错，避免生成 http 链接）。
- 改写后序列化返回。这样管理端只传原始 `latest.json`+文件，无需关心部署域名。

### 4.4 上传处理（POST /api/admin/update）

1. `admin_auth` 已保证是管理员。
2. 解析 multipart：
   - `manifest` 字段 → 校验能解析为 Tauri schema（至少含 `version` + `platforms` 且每平台有 `signature`）→ 临时写 `latest.json.tmp` → rename。
   - `file` 字段 → 必须带 `platform`（须匹配已知键集合白名单：`windows-x86_64`/`windows-aarch64`/`darwin-x86_64`/`darwin-aarch64`/`linux-x86_64` 等）与 `filename` → 写 `<UPDATE_DIR>/files/<platform>/<filename>`（先写 tmp 再 rename）。
3. 安全：
   - `filename` 必须不含 `/`、`\`、`..`，否则拒绝（防目录穿越）。
   - 单文件大小 + 累计大小受 `UPDATE_MAX_UPLOAD_MB` 限制（流式计数，超限中断）。
   - 仅允许 `POST`，且必须带 admin JWT。
4. 成功后返回 `{ok:true, version}`。

### 4.5 存储习惯

沿用 `storage.rs` 既有套路：`create_dir_all` 建目录 → 写 `*.tmp` → `rename` 原子替换，避免半截文件被客户端拉到。

## 5. 管理页（最小 UI）

现有 admin 页由 `rust-embed` 内嵌（`admin.rs` 的 `#[derive(RustEmbed)]` + `/admin`、`/admin/static/:p`）。

- 新增（或扩展）一个"客户端更新"区域：
  - 顶部 `GET /api/admin/update` 拉当前线上版本并展示（版本号 / 发布时间 / 各平台是否已上传）。
  - 一个 multipart 表单：1 个 `latest.json` 文件选择 + 多平台安装包文件选择（每个标注 platform），提交到 `POST /api/admin/update`。
- 实现方式二选一（实现时定）：
  - (a) 在现有内嵌 admin HTML 里加一段 section；
  - (b) 新增内嵌页 `admin_update.html` + 路由 `GET /admin/update` 单独展示。
- 不做复杂进度条，成功/失败用文字提示即可（贴合项目极简 UI 偏好）。

## 6. 配套改动清单

| 文件 | 改动 |
|------|------|
| `server/src/update.rs` | 新增模块（路由 + 改写 + 上传） |
| `server/src/main.rs` | `build_router` 挂载 `/update/*` 与 `/api/admin/update`，注入 `UPDATE_DIR` 等到 `AppState` |
| `server/src/admin.rs` | 提供 `admin_auth` 复用；新增管理页片段/路由 |
| `server/src/state.rs` / `AppState` | 增加 `update_dir: PathBuf`、`update_public_base: Option<String>`、`update_max_upload: u64` |
| `server/clipsync-server.env.example` | 增加 `UPDATE_DIR` / `UPDATE_PUBLIC_BASE` / `UPDATE_MAX_UPLOAD_MB` 三项与注释 |
| `server/Cargo.toml` | 加 multipart / 可能 `tokio-util` 依赖 |
| `server/nginx-clipsync.conf.example` | 增加 `location /update/` 反代到中继 20070 |
| `client/src-tauri/tauri.conf.json` | `plugins.updater.endpoints` 改 `https://<域名>/update/latest.json`（保留 pubkey） |
| `client/src-tauri/src/update/mod.rs` | 实现 `check_update` / `download_and_install`（目前为空壳） |
| `client/src`（前端） | 加"检查更新"入口（设置页按钮 + 可选启动时静默检查） |
| `server/publish-update.sh`（新增） | 本机 `tauri build` → 调 admin 上传 API 把 `latest.json`+安装包推上服务器 |

## 7. nginx 片段（示例，加入 `nginx-clipsync.conf.example`）

```nginx
location /update/ {
    proxy_pass http://127.0.0.1:20070;
    proxy_set_header Host $host;
    proxy_set_header X-Real-IP $remote_addr;
    client_max_body_size 256m;   # 允许上传大安装包
}
```

## 8. 客户端构建前置（必须，否则没产物可传）

当前桌面端用 `tauri build --no-bundle`（本机无 WiX/NSIS），**不出安装包也不出 `latest.json`**。要启用更新必须先：

1. Windows 装 **WiX** + **NSIS** 工具链（或对应平台的打包工具），让 `tauri build` 能出 msi/nsis/app/AppImage。
2. 准备 minisign 密钥对：私钥设 `TAURI_SIGNING_PRIVATE_KEY`（可选 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`）；公钥已配在 `tauri.conf.json` 的 `pubkey`。
3. `tauri build` 自动产出 `target/release/bundle/**` 下的安装包 + `latest.json`（含每平台 `signature`）。
4. `publish-update.sh` 把上述文件通过 admin API 传上服务器。

> 若暂不装打包工具链，可先用"占位 latest.json + 一个小文件"验证服务端收发链路，再补真实构建。

## 9. 验证计划

1. **单元/集成（服务端）**：
   - `cargo test`：url 改写（各 platform 正确、basename 提取、base 回退）。
   - 目录穿越：`/update/files/../secret` 返回 404/拒绝。
   - 上传：用 `curl -F` 模拟 admin 上传 `latest.json`+文件 → `GET /update/latest.json` 返回改写后 url → `GET /update/files/...` 能下载。
   - 未授权：`POST /api/admin/update` 无 JWT 返回 401。
2. **链路冒烟（本机）**：console 模式起 relay，`curl` 验证两个公开端点；admin 页表单上传后版本号正确显示。
3. **客户端**：管理端上传真实构建产物后，客户端"检查更新"能拿到新版本并下载安装（Windows 用 `installMode: passive` 静默安装）。
4. **回归**：`cargo build` / `cargo test` 全绿；中继既有 `/ws`、`/healthz`、`/admin` 不受影响。

## 10. 实施顺序（建议提交粒度）

1. 服务端 `update.rs` + 路由挂载 + `AppState` 配置项 + `env.example`（纯收发，先不起管理页）。
2. 服务端管理上传 API + 安全校验（multipart、白名单、大小限制、原子写）。
3. 管理页 UI（版本展示 + 上传表单）。
4. nginx 片段 + 客户端 `conf.json` endpoint 切换。
5. 客户端 `update/mod.rs` + 前端"检查更新"入口。
6. `publish-update.sh` + 构建前置说明（WiX/NSIS/签名）。
7. 全量自测 + 提交推送（每步验证后 `commit`+`push`，对齐双机联调习惯）。

---

## 待确认/风险

- **管理页实现方式**：内嵌现有页加段 vs 新独立页（实现时选，不影响接口）。
- **`UPDATE_PUBLIC_BASE` 缺失时的回退**：若请求非 https origin，改写会失败返回 500；生产务必配 `UPDATE_PUBLIC_BASE`。
- **大文件上传**：msi/nsis 可能 >100MB，nginx `client_max_body_size` 与 `UPDATE_MAX_UPLOAD_MB` 需对齐，且 axum 侧建议流式计数避免 OOM。
- **多平台发布频率**：管理端一次可传多平台；若只传部分平台，`latest.json` 仍声明全部，缺失平台客户端下载会 404——发布脚本须保证"传齐再换 latest.json"（原子替换已部分覆盖此风险）。
