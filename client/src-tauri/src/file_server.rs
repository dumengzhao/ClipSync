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
    // 读取 HTTP 头直到 \r\n\r\n。
    // 整个头读取限时 10s：否则慢客户端（连上不发数据/逐字节滴数据）会一直
    // 占着并发许可，64 个此类连接即可耗尽 MAX_CONCURRENT_CONNS 拒绝所有入站。
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    let header_len = loop {
        let n = match tokio::time::timeout(std::time::Duration::from_secs(10), sock.read(&mut tmp))
            .await
        {
            Ok(Ok(0)) => return,
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return,
            Err(_) => {
                let _ = write_status(&mut sock, 408, "request timeout").await;
                return;
            }
        };
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 16 * 1024 {
            let _ = write_status(&mut sock, 400, "bad request").await;
            return;
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
        // 1) 网络密钥未就绪 → 直接拒绝（跨 LAN 文件依赖服务端鉴权，无密钥时不外泄）
        let key = *network_key.lock().unwrap();
        let Some(k) = key else {
            let _ = write_status(&mut sock, 503, "encryption key not ready").await;
            return;
        };
        // 2) 请求方凭证：证明其持有同一网络密钥。缺凭证一律 401——
        //    此前只校验 hash 是否登记过，同网段任意主机拿到 hash 就能拉走文件。
        let presented = header_value(&header, "x-clipsync-auth").unwrap_or_default();
        if !crate::crypto::file_auth::verify(&k, hash, &presented) {
            tracing::warn!("拒绝文件拉取：X-Clipsync-Auth 缺失或不匹配（hash={hash}）");
            let _ = write_status(&mut sock, 401, "unauthorized").await;
            return;
        }
        // 3) 凭证通过后才查文件，避免用响应差异探测文件是否存在
        match file_share.get(hash) {
            Some(plain) => {
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
                        // 绝不回退明文：加密失败即失败（早期版本会退回明文返回，与
                        // 本模块「避免明文外泄」的设计意图相悖）
                        tracing::error!("跨 LAN 文件加密失败，已拒绝响应（不回退明文）: {e}");
                        let _ = write_status(&mut sock, 500, "encryption failed").await;
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

/// 从已读入的 HTTP 头文本中取指定请求头（大小写不敏感）。
fn header_value(header: &str, name: &str) -> Option<String> {
    header.lines().skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        if k.trim().eq_ignore_ascii_case(name) {
            Some(v.trim().to_string())
        } else {
            None
        }
    })
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
