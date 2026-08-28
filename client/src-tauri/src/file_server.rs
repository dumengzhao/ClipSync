//! 跨 LAN 文件直取（复用监听端口）
//!
//! 文件 HTTP 端点与 WebSocket 同步服务共用 `listen_port`：由 transfer/manager.rs 的
//! accept 循环在收到 `GET /file/<hash>` 时分流到本模块的 `handle_file_stream`。
//! 对端经 `http://<ext_file_ep>:<listen_port>/file/<hash>` 拉取已复制文件字节，
//! `ext_file_ep` 只是「本机对外可达 IP」的通告（端口恒为 listen_port），不另起服务。

use crate::crypto::aead::{encrypt, NONCE_SIZE};
use crate::file_share::FileShare;
use rand::Rng;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 处理一条已被 accept 循环识别为文件拉取的 TCP 连接（HTTP/1.1 GET /file/<hash>）。
///
/// `network_key` 为跨 LAN 网络密钥（与服务端共享，未连服务端时为 None）：
/// 就绪则对字节做 AES-256-GCM 加密（nonce 12B 前置）再返回；未就绪返回 503，避免明文外泄。
pub async fn handle_file_stream(
    mut sock: TcpStream,
    file_share: Arc<FileShare>,
    network_key: Arc<Mutex<Option<[u8; 32]>>>,
) {
    // 读取 HTTP 头直到 \r\n\r\n
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    let header_len = loop {
        match sock.read(&mut tmp).await {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
                if buf.len() > 16 * 1024 {
                    let _ = write_status(&mut sock, 400, "bad request").await;
                    return;
                }
            }
            Err(_) => return,
        }
    };
    let header = String::from_utf8_lossy(&buf[..header_len]);
    let request_line = header.lines().next().unwrap_or("");
    let mut it = request_line.split_whitespace();
    let method = it.next().unwrap_or("");
    let path = it.next().unwrap_or("");
    if method != "GET" {
        let _ = write_status(&mut sock, 405, "method not allowed").await;
        return;
    }
    if let Some(hash) = path.strip_prefix("/file/") {
        // 去掉查询串 / 片段，仅保留 hash
        let hash = hash.split(['?', '#']).next().unwrap_or("");
        match file_share.get(hash) {
            Some(plain) => {
                let key = *network_key.lock().unwrap();
                match key {
                    Some(k) => {
                        let mut nonce = [0u8; NONCE_SIZE];
                        rand::thread_rng().fill(&mut nonce);
                        match encrypt(&k, &nonce, &plain) {
                            Ok(ct) => {
                                let mut body = Vec::with_capacity(NONCE_SIZE + ct.len());
                                body.extend_from_slice(&nonce);
                                body.extend_from_slice(&ct);
                                let _ = write_body(&mut sock, &body).await;
                            }
                            Err(e) => {
                                tracing::warn!("跨 LAN 文件加密失败，按明文返回: {e}");
                                let _ = write_body(&mut sock, &plain).await;
                            }
                        }
                    }
                    None => {
                        // 跨 LAN 文件依赖服务端鉴权；未连服务端、无密钥时拒绝下载
                        let _ = write_status(&mut sock, 503, "encryption key not ready").await;
                    }
                }
            }
            None => {
                let _ = write_status(&mut sock, 404, "not found").await;
            }
        }
    } else {
        let _ = write_status(&mut sock, 200, "clipsync file server").await;
    }
}

async fn write_status(sock: &mut TcpStream, code: u16, msg: &str) -> std::io::Result<()> {
    let body = msg.as_bytes();
    let head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status_text(code),
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body).await?;
    sock.flush().await
}

async fn write_body(sock: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body).await?;
    sock.flush().await
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Error",
    }
}
