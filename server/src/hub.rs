use crate::models::ServerToClient;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::mpsc;

/// 出站消息：应用层消息 或 协议层 Pong（响应客户端 Ping）。
pub enum OutMsg {
    App(ServerToClient),
    Pong,
}

/// 每个设备一条 mpsc，用于服务端向该设备推送消息。
///
/// **有界队列**：早期用 unbounded，慢客户端（连上却不读 socket）会让服务端推送
/// 无限堆积直到 OOM。容量固定后，队列满即丢弃该条推送——在线通知类消息过时
/// 即失去价值，丢弃比堆积更合理。
pub type Tx = mpsc::Sender<OutMsg>;

/// 每设备出站队列容量。取值远大于正常突发量（一次 offer/节点更新几条消息），
/// 只有真正不消费的客户端才会触顶。
pub const OUT_QUEUE_CAPACITY: usize = 256;

/// 在线连接注册表：device_id -> (network_id, tx)
pub struct Hub {
    conns: Mutex<HashMap<String, (String, Tx)>>,
}

impl Hub {
    pub fn new() -> Self {
        Hub {
            conns: Mutex::new(HashMap::new()),
        }
    }
    pub fn register(&self, device_id: &str, network_id: &str, tx: Tx) {
        self.conns
            .lock()
            .unwrap()
            .insert(device_id.to_string(), (network_id.to_string(), tx));
    }
    pub fn unregister(&self, device_id: &str) {
        self.conns.lock().unwrap().remove(device_id);
    }
    /// 向某设备发应用层消息（自动包装为 OutMsg::App）；返回是否成功（连接存在且未断开）。
    ///
    /// 用 `try_send`：Hub 的调用点都是同步上下文（不能 await），且队列满意味着
    /// 该客户端消费不过来——这时丢弃本条比无限堆积更安全（配合有界队列做背压）。
    pub fn send(&self, device_id: &str, msg: ServerToClient) -> bool {
        let guard = self.conns.lock().unwrap();
        if let Some((_, tx)) = guard.get(device_id) {
            return tx.try_send(OutMsg::App(msg)).is_ok();
        }
        false
    }
}
