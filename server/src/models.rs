use serde::{Deserialize, Serialize};

/// 一个 Network 是一组共享 Token 的设备（跨局域网中枢单元）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Network {
    pub id: String,
    /// SHA-256(Token)，服务端不存明文 Token
    pub token_hash: String,
    pub name: String,
    pub description: String,
    pub nodes: Vec<Node>,
    pub created: i64,
}

/// 网络内的一台设备。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Node {
    pub device_id: String,
    pub name: String,
    /// 局域网分组标识：相同 lan_group 走直连，跨 lan_group 才经服务端中继
    pub lan_group: String,
    /// 对外文件拉取地址 ip:port（公网可达）
    pub ext_file_ep: String,
    pub platform: String,
    /// false = 未启用(pending) / 被禁用；true = 已启用(双向信任，可参与同步)
    pub enabled: bool,
    pub online: bool,
    pub last_seen: i64,
}

/// 设备连服务端时上报的自身信息。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub lan_group: String,
    pub ext_file_ep: String,
    pub platform: String,
}

// ---- 设备侧 → 服务端 消息 ----
#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientToServer {
    Auth { token: String, device: DeviceInfo },
    Heartbeat,
    /// 文字中继（ct = base64 密文，服务端只透传，不解密）
    RelayText { to: String, ct: String },
    /// 文件待复制通知（仅 manifest + 本机 ext_file_ep，服务端不碰字节）
    FileNotify { manifest: serde_json::Value, ext_file_ep: String },
}

// ---- 服务端 → 设备侧 消息 ----
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerToClient {
    Welcome {
        status: String, // "pending" | "active"
        network: NetworkInfo,
        nodes: Vec<NodeInfo>,
    },
    Activated,
    Deactivated,
    NodesUpdate {
        nodes: Vec<NodeInfo>,
    },
    RelayText {
        from: String,
        ct: String,
    },
    FileNotify {
        from: String,
        manifest: serde_json::Value,
        ext_file_ep: String,
    },
    Error {
        code: String,
        msg: String,
    },
}

#[derive(Serialize, Clone, Debug)]
pub struct NetworkInfo {
    pub id: String,
    pub name: String,
}

/// 下发给客户端的节点信息（不含 enabled/online/last_seen 等内部字段）。
#[derive(Serialize, Clone, Debug)]
pub struct NodeInfo {
    pub device_id: String,
    pub name: String,
    pub lan_group: String,
    pub ext_file_ep: String,
    pub platform: String,
}

/// 管理员账号记录（存储于 admin.json）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AdminRecord {
    pub user: String,
    pub pass_hash: String, // argon2
}
