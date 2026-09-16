//! 有界出站队列的**载荷**投递助手。
//!
//! 载荷类消息（剪贴板内容 / 文件清单 / 拉取请求 / 传输完成通知）**不能 `try_send`**：
//! 队列满时丢弃就是「静默丢一次同步」——内容真的没了，日志里也查不到痕迹。
//! 统一语义 = 等待对端消费（背压）→ 超时才放弃并记 WARN。
//!
//! 本模块存在的原因：`transfer::manager`（P2P 直连）与 `server_conn`（跨 LAN 中继）
//! 曾各写一份结构完全相同的实现（连注释都在复述同一件事）。两份实现一旦漂移，
//! 就会出现「一条链路有背压、另一条默默丢包」这种极难排查的不一致。

use tokio::sync::mpsc;

/// 载荷投递默认超时：队列满即视为对端卡死，放弃并记日志。
pub const PAYLOAD_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// **广播**类载荷的超时，比点对点短得多。
///
/// 广播路径（本地剪贴板变化、网格中继转发）是一条**顺序消费**的事件循环，
/// 每次等待都会推迟后续事件的处理；等太久会让 `engine.subscribe()` 的
/// broadcast 积压（容量 64）溢出，`Lagged` 随后把**其它健康对端**的变化整批丢弃。
/// 卡死对端本来就收不到（它的队列已经满），为此牺牲全局不划算，故取 3s。
pub const BROADCAST_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 带超时投递一条载荷，返回是否成功入队。
///
/// - `what`：消息类别（写进日志，便于定位是哪类消息没发出去）
/// - `ctx`：投递目标（对端 device_id / 「中继连接」等，写进日志）
pub async fn send_payload<T>(
    tx: &mpsc::Sender<T>,
    what: &str,
    ctx: &str,
    msg: T,
    timeout: std::time::Duration,
) -> bool {
    match tokio::time::timeout(timeout, tx.send(msg)).await {
        Ok(Ok(())) => true,
        // Closed：接收端已关闭 → 连接断了，属正常路径，降为 debug
        Ok(Err(_)) => {
            tracing::debug!("{ctx} 已断开，{what} 未发送");
            false
        }
        // Full 且等满超时：对端消费不过来（疑似卡死）——本条确实丢弃了，必须 WARN
        Err(_) => {
            tracing::warn!(
                "{ctx} 出站队列 {what} 等待 {}s 仍满，已丢弃（对端疑似卡死）",
                timeout.as_secs()
            );
            false
        }
    }
}
