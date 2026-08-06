//! 传输模块 - 单 WebSocket 通道（信令 + 文件分片复用）

pub mod file_stream;
pub mod manager;
pub mod websocket;

use crate::error::TransferResult;

/// 传输层统一接口
pub trait Transport: Send + Sync {
    /// 发送信令消息（JSON）
    fn send_signaling(&self, msg: &[u8]) -> TransferResult<()>;

    /// 接收信令消息
    fn recv_signaling(&self) -> TransferResult<Vec<u8>>;

    /// 请求文件分片（通过同一 WebSocket 连接的二进制帧）
    fn request_file_chunk(
        &self,
        sync_id: &str,
        file_index: usize,
        offset: u64,
        size: u32,
    ) -> TransferResult<Vec<u8>>;
}
