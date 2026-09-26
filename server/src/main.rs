mod admin;
mod admin_ws;
mod crypto;
mod github_sync;
mod hub;
mod logging;
mod models;
mod state;
mod storage;
mod update;
mod ws;

use crate::state::AppState;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(windows)]
use std::time::Duration;

/// 读取环境变量并初始化存储 / 状态（控制台模式与服务模式共用）。
fn load_state() -> Arc<AppState> {
    let data_dir = std::env::var("CLIPSYNC_DATA_DIR").unwrap_or_else(|_| "data".to_string());
    // 日志先起来：后面所有 println/eprintln 都已换成 tracing，晚初始化会丢掉启动期日志
    logging::init(&data_dir);
    let store = storage::Store::new(PathBuf::from(&data_dir));
    let networks = store.load_networks();

    // 管理员凭据：**只存哈希**，且唯一来源就是 `<data_dir>/admin.json`。
    //
    // 没有这个文件 = 尚未初始化。此时**不中止启动**，而是让服务跑起来、管理 API 全部
    // 返回 409，管理页只显示「初始化」卡片：由管理员粘贴**自己在别处生成**的 Argon2id
    // 哈希串。明文口令从头到尾不经过服务端 —— 环境变量里已经没有 ADMIN_PASS 了。
    //
    // 历史注记：早期版本曾把口令**明文**写进 data/admin.json，后来整个移除、改成只认
    // 环境变量（那又让明文留在 systemd 的 env 文件里）。再到后来是「env 做首次迁移兜底」，
    // 现在兜底也去掉了：明文没有任何一处需要落盘。
    let admin_creds = store.load_admin();
    if admin_creds.is_none() {
        // 0.0.0.0 没法在浏览器里输，展示时换成 127.0.0.1
        let addr = listen_addr().replace("0.0.0.0", "127.0.0.1");
        tracing::warn!(
            "管理员凭据尚未初始化：请访问 http://{addr}/admin 填入你生成的口令哈希串（Argon2id）\
             完成初始化；在此之前管理接口一律拒绝访问。"
        );
    }

    let server_key = store.load_or_create_key().expect("create server key");

    // 更新托管配置（见 UPDATE_MODULE_PLAN.md 第 3 节）
    let update_dir = std::env::var("UPDATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(&data_dir).join("update"));
    let update_public_base = std::env::var("UPDATE_PUBLIC_BASE")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty());
    let update_max_upload = std::env::var("UPDATE_MAX_UPLOAD_MB")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(200)
        * 1024
        * 1024;
    let _ = std::fs::create_dir_all(update_dir.join("files"));

    // 受信反向代理（逗号分隔 IP）：仅这些来源的转发头会被采信（见 admin::client_throttle_key）。
    // 生产形态：异机 1Panel/OpenResty（如 203.0.113.20）→ 本机 20070。
    let trusted_proxies: Vec<std::net::IpAddr> = std::env::var("TRUSTED_PROXIES")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            match s.parse() {
                Ok(ip) => Some(ip),
                Err(_) => {
                    tracing::warn!("TRUSTED_PROXIES 里的 {s:?} 不是合法 IP，已忽略");
                    None
                }
            }
        })
        .collect();
    if !trusted_proxies.is_empty() {
        tracing::info!("受信反向代理: {trusted_proxies:?}（这些来源的 X-Real-IP/XFF 将被采信）");
    }

    Arc::new(AppState {
        store,
        networks: std::sync::Mutex::new(networks),
        hub: hub::Hub::new(),
        admin_ws: std::sync::Mutex::new(std::collections::HashMap::new()),
        server_key,
        admin_creds: std::sync::Mutex::new(admin_creds),
        update_dir,
        update_public_base,
        update_max_upload,
        trusted_proxies,
    })
}

/// 构建 axum 路由（控制台模式与服务模式共用）。
fn build_router(state: Arc<AppState>) -> axum::Router {
    let protected = axum::Router::new()
        .route(
            "/api/admin/networks",
            get(admin::list_networks).post(admin::create_network),
        )
        .route("/api/admin/networks/:id/rename", post(admin::rename_network))
        .route("/api/admin/networks/:id/devices", get(admin::list_devices))
        .route(
            "/api/admin/networks/:id/devices/:dev/enable",
            post(admin::enable_device_handler),
        )
        .route(
            "/api/admin/networks/:id/devices/:dev/disable",
            post(admin::disable_device_handler),
        )
        .route(
            "/api/admin/networks/:id/devices/:dev/remove",
            post(admin::remove_device_handler),
        )
        .route("/api/admin/networks/:id/removed", get(admin::list_removed_handler))
        .route(
            "/api/admin/networks/:id/removed/:dev/restore",
            post(admin::restore_device_handler),
        )
        .route(
            "/api/admin/networks/:id/removed/:dev/purge",
            post(admin::purge_device_handler),
        )
        .route(
            "/api/admin/update",
            get(update::admin_info).post(update::admin_upload),
        )
        .route("/api/admin/update/history", get(update::admin_history))
        .route(
            "/api/admin/update/downloads",
            post(update::admin_set_downloads),
        )
        .route("/api/admin/password", post(admin::change_password))
        // 上传安装包可能 >2MB，须放开 axum 默认的 2MB DefaultBodyLimit
        // （否则 Multipart extractor 内部 with_limited_body 会在 2MB 处截断报错）
        .layer(axum::extract::DefaultBodyLimit::max(
            state.update_max_upload.min(usize::MAX as u64) as usize,
        ))
        // GitHub 同步：手动触发 / 读配置 / 改开关（都在 admin_auth 之下）
        .route("/api/admin/github-sync", get(github_sync::admin_get))
        .route("/api/admin/github-sync/run", post(github_sync::admin_run))
        .route(
            "/api/admin/github-sync/test",
            post(github_sync::admin_test),
        )
        .route(
            "/api/admin/github-sync/config",
            post(github_sync::admin_set_config),
        )
        .route_layer(from_fn_with_state(state.clone(), admin::admin_auth));

    axum::Router::new()
        .route("/ws", get(ws::device_ws))
        .route("/healthz", get(|| async { "ok" }))
        .route("/", get(redirect_root))
        .route("/api/admin/login", post(admin::admin_login))
        // 未初始化时才用到的两个端点（公开）：前端据此决定显示登录卡片还是初始化卡片
        .route("/api/admin/init-status", get(admin::admin_init_status))
        .route("/api/admin/init", post(admin::admin_init))
        .route("/api/admin/ws", get(admin_ws::admin_ws))
        .route("/admin", get(admin::admin_page))
        .route("/admin/static/:p", get(admin::admin_static))
        .route("/update/latest.json", get(update::latest_json))
        .route("/update/files/:platform/:file", get(update::download_file))
        // 公开下载页：不登录就能看当前版本（历史版本由管理页开关控制）
        .route("/update/versions.json", get(update::public_versions))
        .route("/downloads", get(update::downloads_page))
        .merge(protected)
        .with_state(state.clone())
}

/// GET /：根路径重定向到管理后台，访问 host:port/ 也能跳到 admin 页面。
async fn redirect_root() -> axum::response::Redirect {
    axum::response::Redirect::to("/admin")
}

fn listen_addr() -> String {
    std::env::var("LISTEN").unwrap_or_else(|_| "0.0.0.0:20070".to_string())
}

/// 启动 axum 服务，直到 shutdown future 触发才优雅退出。
async fn serve(
    state: Arc<AppState>,
    router: axum::Router,
    listen: String,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) {
    // GitHub 同步的定时轮询：未配置 GITHUB_REPO 时内部立即返回，几乎零开销。
    // 放在 serve 里而不是 load_state 里：服务模式下的 load_state 跑在 tokio 运行时之外，
    // 在那里 tokio::spawn 会 panic。
    github_sync::spawn_ticker(state.clone());
    // 日志定期清理：长跑的服务不能只在启动时清一次（否则一个月下来三十个文件、一年几百个）
    {
        let data_dir = state.store.dir.to_string_lossy().to_string();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(logging::PRUNE_INTERVAL).await;
                logging::prune(&data_dir);
            }
        });
    }
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .expect("bind listen addr");
    tracing::info!("listening on {listen}");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
    .expect("serve");
}

/// `clipsync-server hash` 的用法说明。
const HASH_USAGE: &str = "\
Usage: clipsync-server hash [--stdin]

Generate an Argon2id hash string for the admin password (used by the
first-time setup page at http://<server>:20070/admin).

  --stdin   Read two lines from stdin (password / confirm). For scripts.
            Default is interactive input with echo disabled.

Examples:
  clipsync-server hash
  printf '%s\\n%s\\n' \"$PW\" \"$PW\" | clipsync-server hash --stdin

Output: the hash string on stdout, nothing else.";

/// `clipsync-server hash` —— 交互式生成管理员口令的 Argon2id 哈希串。
///
/// 对应 vaultwarden 的 `docker run ... /vaultwarden hash`：让人不必去找第三方工具，
/// 用服务端自己的二进制就能生成**和运行时校验完全同一套参数**的哈希串
/// （自己算的最大好处是参数不会跟服务端对不上）。
///
/// 交互读、终端不回显，所以明文口令既不进 shell history，也不落任何文件；
/// 哈希串打到 **stdout**（可重定向），提示信息走 stderr。
/// 读一次口令。`from_stdin=true` 时走普通 stdin（脚本/管道场景，无法隐藏回显），
/// 否则走终端隐藏回显 —— 后者要求**真的有个终端**：用管道喂会一直等键盘输入，
/// 所以脚本里必须显式加 `--stdin`。
fn read_secret(prompt: &str, from_stdin: bool) -> std::io::Result<String> {
    if from_stdin {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        Ok(s.trim_end_matches(['\r', '\n']).to_string())
    } else {
        rpassword::prompt_password(prompt)
    }
}

fn cmd_hash() -> i32 {
    // --stdin：从标准输入读口令（脚本用）。注意此时无法隐藏回显，
    // 也就意味着口令可能出现在管道或 shell history 里 —— 只为自动化保留。
    let from_stdin = std::env::args().any(|a| a == "--stdin");
    // 输出一律用 ASCII：中文在 Windows 控制台（GBK 代码页）会显示成乱码。
    let first = match read_secret("Password: ", from_stdin) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read password: {e} (use --stdin in scripts)");
            return 1;
        }
    };
    if first.trim().len() < storage::MIN_PASSWORD_LEN {
        eprintln!(
            "error: password too short (minimum {} characters)",
            storage::MIN_PASSWORD_LEN
        );
        return 1;
    }
    if first.trim() == storage::DEFAULT_PASSWORD {
        eprintln!("error: password must not be the default one");
        return 1;
    }
    let second = match read_secret("Entry Password: ", from_stdin) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read password: {e}");
            return 1;
        }
    };
    if first != second {
        eprintln!("error: the two entries do not match");
        return 1;
    }
    // stdout 只有这一行哈希串，方便重定向 / 直接粘贴
    println!("{}", storage::hash_pass(&first));
    0
}

#[tokio::main]
async fn main() {
    // 子命令：不碰存储与网络，先处理掉（`hash` 需要在没有数据目录的机器上也能跑）
    if std::env::args().nth(1).as_deref() == Some("hash") {
        let rest: Vec<String> = std::env::args().skip(2).collect();
        if rest.iter().any(|a| a == "--help" || a == "-h") {
            println!("{}", HASH_USAGE);
            return;
        }
        // 未知参数直接报错，别默默忽略 —— 否则多打一个字符就变成「等键盘输入」的假死
        if let Some(bad) = rest.iter().find(|a| *a != "--stdin") {
            eprintln!("[clipsync-server] 无法识别的参数：{bad}\n\n{HASH_USAGE}");
            std::process::exit(1);
        }
        std::process::exit(cmd_hash());
    }
    // Windows 服务模式：由 SCM 以 --service 启动，交给 service dispatcher。
    #[cfg(windows)]
    {
        if std::env::args().any(|a| a == "--service") {
            if let Err(e) =
                windows_service::service_dispatcher::start("ClipSyncServer", service_main_wrapper)
            {
                eprintln!("[clipsync-server] service dispatcher error: {e}");
                std::process::exit(1);
            }
            return;
        }
    }

    let state = load_state();
    let router = build_router(state.clone());
    let listen = listen_addr();
    serve(state, router, listen, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await;
}

#[cfg(windows)]
fn service_main(_args: Vec<std::ffi::OsString>) {
    if let Err(e) = run_service() {
        eprintln!("[clipsync-server] service error: {e}");
    }
}

// 生成符合 SCM 调用约定的 extern "system" 包装函数（service_dispatcher::start 需要该签名）
#[cfg(windows)]
windows_service::define_windows_service!(service_main_wrapper, service_main);

#[cfg(windows)]
fn run_service() -> windows_service::Result<()> {
    use windows_service::service::*;
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    // 收到 SCM 的 Stop / Shutdown 时通知 axum 优雅退出
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let mut shutdown_tx = Some(shutdown_tx);

    let event_handler = move |control_event| match control_event {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Some(tx) = shutdown_tx.take() {
                let _ = tx.send(());
            }
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    };

    let status_handle = service_control_handler::register("ClipSyncServer", event_handler)?;

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    let state = load_state();
    let router = build_router(state.clone());
    let listen = listen_addr();

    let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
    rt.block_on(serve(state, router, listen, async {
        let _ = shutdown_rx.await;
    }));

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}
