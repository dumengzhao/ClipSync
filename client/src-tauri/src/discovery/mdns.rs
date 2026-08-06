//! mDNS 局域网自动发现
//!
//! 通过 DNS-SD（Bonjour / mDNS）在同一网段内广播本机服务并发现其它 ClipSync 实例。
//!
//! **关键设计：发现不依赖任何写死的端口。**
//! 每个实例通过 mDNS 广告自己的「真实监听端口」（取自配置，默认 24681，可在配置中修改），
//! 发现方从对端广告的 SRV 记录里**动态读取端口**，因此改端口无需改动发现逻辑，
//! 也不会出现「扫固定端口」这种写死行为。
//!
//! - 服务类型：`_clipsync._tcp.local`
//! - TXT 记录：`device_id`、`device_name`
//! - 仅在 `mdns` feature 开启时启用真实实现；关闭时提供无操作占位。

use crate::device::identity::DeviceIdentity;
use tauri::AppHandle;

/// mDNS 服务类型（DNS-SD）
pub const SERVICE_TYPE: &str = "_clipsync._tcp.local.";

#[cfg(feature = "mdns")]
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
#[cfg(feature = "mdns")]
use parking_lot::Mutex;
#[cfg(feature = "mdns")]
use std::collections::HashMap;
#[cfg(feature = "mdns")]
use std::net::{IpAddr, UdpSocket};
#[cfg(feature = "mdns")]
use tauri::{Emitter, Manager};

#[cfg(feature = "mdns")]
use crate::AppState;

/// 局域网发现控制器
///
/// 内部用 `Mutex<Option<ServiceDaemon>>` 持有 mDNS 守护进程，支持启动 / 重新配置 / 关停。
#[cfg(feature = "mdns")]
pub struct MdnsDiscovery {
    service_type: &'static str,
    daemon: Mutex<Option<ServiceDaemon>>,
}

#[cfg(feature = "mdns")]
impl Default for MdnsDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "mdns")]
impl MdnsDiscovery {
    pub fn new() -> Self {
        Self {
            service_type: SERVICE_TYPE,
            daemon: Mutex::new(None),
        }
    }

    /// 开始广播本机，并订阅局域网内其它实例。
    ///
    /// `port` 取自已加载配置（默认 24681，可在配置中修改）。发现方会从本机广告的
    /// SRV 记录里读到这个端口，因此发现逻辑本身不写死任何端口。
    pub fn start(
        &self,
        app: &AppHandle,
        identity: &DeviceIdentity,
        port: u16,
    ) -> anyhow::Result<()> {
        let mut guard = self.daemon.lock();
        if guard.is_some() {
            return Ok(()); // 已在运行
        }

        let daemon =
            ServiceDaemon::new().map_err(|e| anyhow::anyhow!("mDNS daemon init failed: {e}"))?;

        let instance = format!("clipsync-{}", identity.id.0);
        // 取本机用于访问外网的 IPv4 作为广播地址；离线时退化为回环地址（无害）
        let ip = local_ipv4().unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));

        // TXT 记录携带设备标识；端口不在此写死，由对端从 SRV 记录读取。
        // 设备名取运行时配置（默认即本机机器名），改名后重广播即生效。
        let device_name = app
            .state::<AppState>()
            .config
            .lock()
            .device_name
            .clone();
        let mut props = HashMap::new();
        props.insert("device_id".to_string(), identity.id.0.clone());
        props.insert("device_name".to_string(), device_name);

        // host 必须以 `.local.` 结尾，否则 mdns-sd 拒绝注册
        let host = format!("{instance}.local.");
        let service = ServiceInfo::new(self.service_type, &instance, &host, ip, port, props)
            .map_err(|e| anyhow::anyhow!("mDNS service build failed: {e}"))?;

        daemon
            .register(service)
            .map_err(|e| anyhow::anyhow!("mDNS register failed: {e}"))?;

        let receiver = daemon
            .browse(self.service_type)
            .map_err(|e| anyhow::anyhow!("mDNS browse failed: {e}"))?;

        *guard = Some(daemon);
        drop(guard);

        let app2 = app.clone();
        let my_id = identity.id.0.clone();
        std::thread::spawn(move || {
            while let Ok(ev) = receiver.recv() {
                match ev {
                    ServiceEvent::ServiceResolved(info) => {
                        let Some(did) = info.get_property_val_str("device_id") else {
                            continue;
                        };
                        // 跳过本机自己
                        if did == my_id {
                            continue;
                        }
                        // 优先选择非回环 IPv4，否则退而取任意地址
                        // get_addresses() 返回 IpAddr 集合（端口另由 get_port() 提供）
                        let peer_addr = info
                            .get_addresses()
                            .iter()
                            .find(|a| a.is_ipv4() && !a.is_loopback())
                            .or_else(|| info.get_addresses().iter().next())
                            .map(|a| a.to_string())
                            .unwrap_or_default();

                        let peer = crate::discovery::DiscoveredPeer {
                            device_id: did.to_string(),
                            device_name: info
                                .get_property_val_str("device_name")
                                .unwrap_or("")
                                .to_string(),
                            addr: peer_addr,
                            // 端口来自对端广告的 SRV 记录，而非写死常量
                            port: info.get_port(),
                        };

                        // 写入共享存储，供 list_discovered_peers 查询
                        let st = app2.state::<crate::AppState>();
                        st.discovered
                            .lock()
                            .insert(peer.device_id.clone(), peer.clone());
                        // 通知前端实时刷新
                        let _ = app2.emit("peer-discovered", &peer);
                    }
                    ServiceEvent::ServiceRemoved(full, _) => {
                        // full = "clipsync-<device_id>._clipsync._tcp.local."
                        let did = full
                            .strip_prefix("clipsync-")
                            .and_then(|s| s.strip_suffix("._clipsync._tcp.local."))
                            .unwrap_or(&full)
                            .to_string();
                        let st = app2.state::<crate::AppState>();
                        st.discovered.lock().remove(&did);
                        let _ = app2.emit("peer-lost", &did);
                    }
                    _ => {}
                }
            }
        });

        Ok(())
    }

    /// 端口变更后重新广播：先关停旧守护进程，再按新端口注册。
    pub fn reconfigure(
        &self,
        app: &AppHandle,
        identity: &DeviceIdentity,
        port: u16,
    ) -> anyhow::Result<()> {
        self.shutdown();
        self.start(app, identity, port)
    }

    /// 关停 mDNS 守护进程（进程退出或配置关闭时调用）
    pub fn shutdown(&self) {
        let mut guard = self.daemon.lock();
        if let Some(d) = guard.take() {
            let _ = d.shutdown();
        }
    }
}

/// 取得本机用于访问外网的 IPv4 地址（局域网出口 IP）。
///
/// 通过向一个公网地址发起 UDP connect（不真正发包）来读取本地出口地址，
/// 是一种常见的、无需额外依赖的获取局域网 IP 的技巧。
#[cfg(feature = "mdns")]
fn local_ipv4() -> Option<IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

// ---------------------------------------------------------------------------
// 非 mdns 构建：无操作占位，保证在不启用 mDNS 的编译下行为一致、可正常构建
// ---------------------------------------------------------------------------

#[cfg(not(feature = "mdns"))]
pub struct MdnsDiscovery;

#[cfg(not(feature = "mdns"))]
impl Default for MdnsDiscovery {
    fn default() -> Self {
        Self
    }
}

#[cfg(not(feature = "mdns"))]
impl MdnsDiscovery {
    pub fn new() -> Self {
        Self
    }

    pub fn start(
        &self,
        _app: &AppHandle,
        _identity: &DeviceIdentity,
        _port: u16,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn reconfigure(
        &self,
        _app: &AppHandle,
        _identity: &DeviceIdentity,
        _port: u16,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn shutdown(&self) {}
}
