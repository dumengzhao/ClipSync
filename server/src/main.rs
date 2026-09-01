mod admin;
mod admin_ws;
mod crypto;
mod hub;
mod models;
mod state;
mod storage;
mod ws;

use crate::models::AdminRecord;
use crate::state::AppState;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// 读取环境变量并初始化存储 / 状态（控制台模式与服务模式共用）。
fn load_state() -> Arc<AppState> {
    let data_dir = std::env::var("CLIPSYNC_DATA_DIR").unwrap_or_else(|_| "data".to_string());
    let admin_user = std::env::var("ADMIN_USER").unwrap_or_else(|_| "admin".to_string());
    let admin_pass = std::env::var("ADMIN_PASS").unwrap_or_else(|_| "clipsync".to_string());
    let store = storage::Store::new(PathBuf::from(&data_dir));
    let networks = store.load_networks();
    let (admin_user, admin_pass_hash) = match store.load_admin() {
        Some(a) => (a.user, a.pass_hash),
        None => {
            let h = storage::hash_pass(&admin_pass);
            let _ = store.save_admin(&AdminRecord {
                user: admin_user.clone(),
                pass_hash: h.clone(),
            });
            (admin_user.clone(), h)
        }
    };
    let server_key = store.load_or_create_key().expect("create server key");

    Arc::new(AppState {
        store,
        networks: std::sync::Mutex::new(networks),
        hub: hub::Hub::new(),
        admin_ws: std::sync::Mutex::new(std::collections::HashMap::new()),
        server_key,
        admin_user,
        admin_pass_hash,
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
        .route_layer(from_fn_with_state(state.clone(), admin::admin_auth));

    axum::Router::new()
        .route("/ws", get(ws::device_ws))
        .route("/healthz", get(|| async { "ok" }))
        .route("/", get(redirect_root))
        .route("/api/admin/login", post(admin::admin_login))
        .route("/api/admin/ws", get(admin_ws::admin_ws))
        .route("/admin", get(admin::admin_page))
        .route("/admin/static/:p", get(admin::admin_static))
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
    router: axum::Router,
    listen: String,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) {
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .expect("bind listen addr");
    println!("[clipsync-server] listening on {listen}");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .expect("serve");
}

#[tokio::main]
async fn main() {
    // Windows 服务模式：由 SCM 以 --service 启动，交给 service dispatcher。
    #[cfg(windows)]
    {
        if std::env::args().any(|a| a == "--service") {
            if let Err(e) = windows_service::service_dispatcher::start("ClipSyncServer", service_main_wrapper)
            {
                eprintln!("[clipsync-server] service dispatcher error: {e}");
                std::process::exit(1);
            }
            return;
        }
    }

    let state = load_state();
    let router = build_router(state);
    let listen = listen_addr();
    serve(router, listen, async {
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
    let router = build_router(state);
    let listen = listen_addr();

    let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
    rt.block_on(serve(router, listen, async {
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
