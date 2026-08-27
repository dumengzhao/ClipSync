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

#[tokio::main]
async fn main() {
    let data_dir = std::env::var("CLIPSYNC_DATA_DIR").unwrap_or_else(|_| "data".to_string());
    let admin_user = std::env::var("ADMIN_USER").unwrap_or_else(|_| "admin".to_string());
    let admin_pass = std::env::var("ADMIN_PASS").unwrap_or_else(|_| "clipsync".to_string());
    let listen = std::env::var("LISTEN").unwrap_or_else(|_| "0.0.0.0:20070".to_string());

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

    let state = Arc::new(AppState {
        store,
  networks: std::sync::Mutex::new(networks),
        hub: hub::Hub::new(),
        admin_ws: std::sync::Mutex::new(std::collections::HashMap::new()),
        server_key,
        admin_user,
        admin_pass_hash,
    });

    let protected = axum::Router::new()
        .route(
            "/api/admin/networks",
            get(admin::list_networks).post(admin::create_network),
        )
        .route(
            "/api/admin/networks/:id/rename",
            post(admin::rename_network),
        )
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
        .route(
            "/api/admin/networks/:id/removed",
            get(admin::list_removed_handler),
        )
        .route(
            "/api/admin/networks/:id/removed/:dev/restore",
            post(admin::restore_device_handler),
        )
        .route_layer(from_fn_with_state(state.clone(), admin::admin_auth));

    let app = axum::Router::new()
        .route("/ws", get(ws::device_ws))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/admin/login", post(admin::admin_login))
        .route("/api/admin/ws", get(admin_ws::admin_ws))
        .route("/admin", get(admin::admin_page))
        .route("/admin/static/:p", get(admin::admin_static))
        .merge(protected)
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(&listen).await.expect("bind listen addr");
    println!("[clipsync-server] listening on {listen}");
    axum::serve(listener, app).await.expect("serve");
}
