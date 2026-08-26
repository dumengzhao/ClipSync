//! 内嵌 HTTP 文件服务（跨 LAN 文件直取）
//!
//! 监听本机 `ext_file_ep` 端口，对端经 `http://<ext_file_ep>/file/<hash>` 拉取已复制的
//! 文件字节。所有逻辑由 `file_share` 注册表按 hash 提供，服务端不接触任何字节。

use crate::file_share::FileShare;
use std::sync::Arc;
use std::thread;

/// 启动跨 LAN 文件 HTTP 服务（独立线程；失败仅记录不阻断启动）。
pub fn start_file_server(file_share: Arc<FileShare>, ep: String) {
    if ep.trim().is_empty() {
        tracing::info!("未配置 ext_file_ep，跨 LAN 文件拉取不可用");
        return;
    }
    let port = match ep.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) {
        Some(p) => p,
        None => {
            tracing::warn!("ext_file_ep 端口解析失败: {ep}");
            return;
        }
    };
    let host = ep
        .rsplit_once(':')
        .map(|(h, _)| h.to_string())
        .unwrap_or_default();
    let addr = if host.is_empty() {
        format!("0.0.0.0:{port}")
    } else {
        format!("{host}:{port}")
    };

    thread::spawn(move || {
        let server = match tiny_http::Server::http(&addr) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("跨 LAN 文件服务绑定 {addr} 失败: {e}");
                return;
            }
        };
        tracing::info!("跨 LAN 文件服务已启动: {addr}");
        for req in server.incoming_requests() {
            let url = req.url().to_string();
            if let Some(hash) = url.strip_prefix("/file/") {
                match file_share.get(hash) {
                    Some(bytes) => {
                        let mut resp = tiny_http::Response::from_data(bytes);
                        resp.add_header(
                            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/octet-stream"[..])
                                .unwrap(),
                        );
                        let _ = req.respond(resp);
                    }
                    None => {
                        let _ = req.respond(
                            tiny_http::Response::from_string("not found").with_status_code(404),
                        );
                    }
                }
            } else {
                let _ = req.respond(tiny_http::Response::from_string("clipsync file server").with_status_code(200));
            }
        }
    });
}
