//! 内嵌 HTTP 文件服务（跨 LAN 文件直取）
//!
//! 监听本机 `ext_file_ep` 端口，对端经 `http://<ext_file_ep>/file/<hash>` 拉取已复制的
//! 文件字节。所有逻辑由 `file_share` 注册表按 hash 提供，服务端不接触任何字节。

use crate::crypto::aead::{encrypt, NONCE_SIZE};
use crate::file_share::FileShare;
use rand::Rng;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;

/// 启动跨 LAN 文件 HTTP 服务（独立线程；失败仅记录不阻断启动）。
///
/// `network_key` 为跨 LAN 网络密钥（与服务端共享，未连服务端时为 None）。
/// 命中文件时：密钥就绪则对字节做 AES-256-GCM 加密（nonce 12B 前置）再返回；
/// 密钥未就绪（未连服务端）返回 503，避免明文外泄。
pub fn start_file_server(
    file_share: Arc<FileShare>,
    network_key: Arc<Mutex<Option<[u8; 32]>>>,
    ep: String,
) {
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
                    Some(plain) => {
                        let key = *network_key.lock().unwrap();
                        let body = match key {
                            Some(k) => {
                                let mut nonce = [0u8; NONCE_SIZE];
                                rand::thread_rng().fill(&mut nonce);
                                match encrypt(&k, &nonce, &plain) {
                                    Ok(ct) => {
                                        let mut v = Vec::with_capacity(NONCE_SIZE + ct.len());
                                        v.extend_from_slice(&nonce);
                                        v.extend_from_slice(&ct);
                                        v
                                    }
                                    Err(e) => {
                                        tracing::warn!("跨 LAN 文件加密失败，按明文返回: {e}");
                                        plain
                                    }
                                }
                            }
                            None => {
                                // 跨 LAN 文件依赖服务端鉴权；未连服务端、无密钥时拒绝下载
                                let _ = req.respond(
                                    tiny_http::Response::from_string("encryption key not ready")
                                        .with_status_code(503),
                                );
                                continue;
                            }
                        };
                        let mut resp = tiny_http::Response::from_data(body);
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
