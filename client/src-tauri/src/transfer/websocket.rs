//! WebSocket 单通道（信令 + 文件分片复用）
//!
//! 阶段一实现：基于 tokio-tungstenite 的双向通信
//! 消息复用：二进制帧首字节区分类型（信令 JSON / 文件分片 Bincode / 心跳）
