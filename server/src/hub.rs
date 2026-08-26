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
pub type Tx = mpsc::UnboundedSender<OutMsg>;

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
    pub fn send(&self, device_id: &str, msg: ServerToClient) -> bool {
        let guard = self.conns.lock().unwrap();
        if let Some((_, tx)) = guard.get(device_id) {
            return tx.send(OutMsg::App(msg)).is_ok();
        }
        false
    }
}
