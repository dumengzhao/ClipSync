use serde::{Deserialize, Serialize};

/// 一个 Network 是一组共享 Token 的设备（跨局域网中枢单元）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Network {
    pub id: String,
    /// SHA-256(Token)，用于客户端鉴权校验
    pub token_hash: String,
    /// 明文 Token：为便于管理后台查看/复制而持久化（仅管理员可读，列表接口返回）
    #[serde(default)]
    pub token: String,
    pub name: String,
    pub description: String,
    pub nodes: Vec<Node>,
    /// 被管理员移除（拉黑）的设备；其重连鉴权将被拒绝，直至被「恢复」才允许重新配对。
    #[serde(default)]
    pub removed_devices: Vec<RemovedDevice>,
    pub created: i64,
}

/// 被管理员从网络移除（拉黑）的设备记录。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RemovedDevice {
    pub device_id: String,
    /// 移除时记录的设备名，仅用于管理页展示
    #[serde(default)]
    pub name: String,
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
    /// 操作系统版本号（如 macOS 14.5 / Windows 11），由客户端上报
    #[serde(default)]
    pub os_version: String,
    /// 硬件级唯一标识（跨平台指纹，优先于 device_id 展示）
    #[serde(default)]
    pub hardware_id: String,
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
    /// 操作系统版本号（如 macOS 14.5 / Windows 11），由客户端上报
    #[serde(default)]
    pub os_version: String,
    /// 硬件级唯一标识（跨平台指纹），缺失时服务端以 device.id 兜底
    #[serde(default)]
    pub hardware_id: String,
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
    /// 设备已被管理员从网络移除（拉黑）：客户端应停止重连并提示需重新配对
    Removed,
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
    /// 硬件级唯一标识（跨平台指纹）
    #[serde(default)]
    pub hardware_id: String,
}

/// 管理员账号记录（存储于 admin.json）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AdminRecord {
    pub user: String,
    pub pass_hash: String, // argon2
}
