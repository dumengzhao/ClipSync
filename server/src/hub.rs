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
    /// 用 `try_send`：只适用于**通知/快照类**消息（Welcome、Error、激活状态、
    /// 节点列表快照——过时即失效，且下一次变更会立即重发）。
    /// **载荷类消息（中继的剪贴板内容、文件通知）必须用 `send_payload`**：
    /// 它们承载实际数据，队列满时丢弃等于「静默丢一次同步」。
    pub fn send(&self, device_id: &str, msg: ServerToClient) -> bool {
        let guard = self.conns.lock().unwrap();
        if let Some((_, tx)) = guard.get(device_id) {
            return tx.try_send(OutMsg::App(msg)).is_ok();
        }
        false
    }

    /// 向某设备投递**载荷类**消息：队列满时等待消费（背压），超时才放弃并记日志。
    ///
    /// 此前中继剪贴板/文件通知走 `try_send` 且**丢弃返回值**——队列满（客户端卡住）
    /// 时对端完全收不到，也没有任何日志，表现为「偶发不同步，查无痕迹」。
    pub async fn send_payload(&self, device_id: &str, msg: ServerToClient) -> bool {
        // 锁内只取 tx 的克隆，绝不跨 await 持锁（std Mutex 跨 await 会阻塞整个 runtime）
        let tx = {
            let guard = self.conns.lock().unwrap();
            match guard.get(device_id) {
                Some((_, tx)) => tx.clone(),
                None => return false,
            }
        };
        match tokio::time::timeout(PAYLOAD_SEND_TIMEOUT, tx.send(OutMsg::App(msg))).await {
            Ok(Ok(())) => true,
            Ok(Err(_)) => false,
            Err(_) => {
                eprintln!(
                    "[clipsync-server] 设备 {device_id} 出站队列等待 {}s 仍满，本条载荷已丢弃（客户端疑似卡死）",
                    PAYLOAD_SEND_TIMEOUT.as_secs()
                );
                false
            }
        }
    }
}

/// 载荷类消息的投递超时：与客户端侧 `send_payload` 的语义一致（队列满→背压→超时放弃）。
const PAYLOAD_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
