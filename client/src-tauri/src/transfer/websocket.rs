//! WebSocket 单通道（信令 + 文件分片复用）
//!
//! 二进制帧首字节区分消息类型：信令 JSON / 文件分片请求(Bincode) / 文件分片响应 / 心跳。
//! 连接管理（connect / listen，基于 tokio-tungstenite）为阶段二实现，见模块底部说明。

use crate::error::TransferError;
use serde::Serialize;

/// 消息类型（二进制帧首字节）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    Signal = 0x01,
    FileChunkRequest = 0x02,
    FileChunkResponse = 0x03,
    Heartbeat = 0x04,
}

impl MessageType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(Self::Signal),
            0x02 => Some(Self::FileChunkRequest),
            0x03 => Some(Self::FileChunkResponse),
            0x04 => Some(Self::Heartbeat),
            _ => None,
        }
    }
}

/// 消息帧：类型标签 + 载荷
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageFrame {
    pub msg_type: MessageType,
    pub payload: Vec<u8>,
}

impl MessageFrame {
    pub fn new(msg_type: MessageType, payload: Vec<u8>) -> Self {
        Self { msg_type, payload }
    }

    /// 编码为线上字节：[1 byte type][payload...]
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.payload.len() + 1);
        buf.push(self.msg_type as u8);
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// 从线上字节解码；首字节非法或为空返回错误
    pub fn decode(buf: &[u8]) -> Result<Self, TransferError> {
        if buf.is_empty() {
            return Err(TransferError::Handshake("empty frame".into()));
        }
        let msg_type = MessageType::from_u8(buf[0])
            .ok_or_else(|| TransferError::Handshake(format!("unknown msg type {}", buf[0])))?;
        Ok(Self {
            msg_type,
            payload: buf[1..].to_vec(),
        })
    }

    /// 便捷构造：将可序列化信令消息 JSON 编码后打包为 Signal 帧
    pub fn signal_json<T: Serialize>(value: &T) -> Result<Self, TransferError> {
        let payload =
            serde_json::to_vec(value).map_err(|e| TransferError::Handshake(e.to_string()))?;
        Ok(Self::new(MessageType::Signal, payload))
    }
}

// 阶段二（connect / listen）实现要点：
// - `connect(addr)`：tokio_tungstenite::connect_async(ws://addr) 返回 (WebSocketStream, _)
// - `listen(port)`：tokio TcpListener 接受连接，对每个连接 upgrade 为 WebSocket
// - 每条消息按 `MessageFrame` 编解码；信令 JSON、文件分片 Bincode 复用同一二进制流
// - 多文件并发通过 `sync_id + file_index + offset` 在同一连接上多路复用

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let frame = MessageFrame::new(MessageType::Signal, b"hello".to_vec());
        let bytes = frame.encode();
        assert_eq!(bytes[0], 0x01);
        let decoded = MessageFrame::decode(&bytes).unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn decode_rejects_empty() {
        assert!(MessageFrame::decode(&[]).is_err());
    }

    #[test]
    fn decode_rejects_unknown_type() {
        assert!(MessageFrame::decode(&[0x09, 0x00]).is_err());
    }

    #[test]
    fn type_from_u8() {
        assert_eq!(MessageType::from_u8(0x01), Some(MessageType::Signal));
        assert_eq!(MessageType::from_u8(0x04), Some(MessageType::Heartbeat));
        assert_eq!(MessageType::from_u8(0x00), None);
    }

    #[test]
    fn signal_json_roundtrip() {
        #[derive(Serialize)]
        struct Greeting {
            hello: &'static str,
        }
        let frame = MessageFrame::signal_json(&Greeting { hello: "world" }).unwrap();
        assert_eq!(frame.msg_type, MessageType::Signal);
        let decoded = MessageFrame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.msg_type, MessageType::Signal);
    }
}
