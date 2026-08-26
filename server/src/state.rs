use crate::crypto::hash_token;
use crate::hub::{Hub, OutMsg, Tx};
use crate::models::*;
use crate::storage::Store;
use anyhow::Result;
use std::sync::Mutex;

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct AppState {
    pub store: Store,
    pub networks: Mutex<Vec<Network>>,
    pub hub: Hub,
    /// 会话签名密钥：登录签发/HMAC 校验 admin 会话
    pub server_key: String,
    pub admin_user: String,
    pub admin_pass_hash: String,
}

impl AppState {
    pub fn save(&self) -> Result<()> {
        let nets = self.networks.lock().unwrap();
        self.store.save_networks(&nets)
    }

    fn enabled_node_infos(net: &Network) -> Vec<NodeInfo> {
        net.nodes
            .iter()
            .filter(|n| n.enabled)
            .map(|n| NodeInfo {
                device_id: n.device_id.clone(),
                name: n.name.clone(),
                lan_group: n.lan_group.clone(),
                ext_file_ep: n.ext_file_ep.clone(),
                platform: n.platform.clone(),
            })
            .collect()
    }

    /// 设备鉴权：Token 哈希命中则注册/更新节点（enabled 保持原值，首次为 false）。
    /// 成功返回 (network_id, device_id)。
    pub fn handle_auth(
        &self,
        token: &str,
        device: &DeviceInfo,
        tx: &Tx,
    ) -> Result<(String, String), String> {
        let token_hash = hash_token(token);
        let mut nets = self.networks.lock().unwrap();
        let idx = nets
            .iter()
            .position(|n| n.token_hash == token_hash)
            .ok_or_else(|| "bad_token".to_string())?;
        let net_id = nets[idx].id.clone();
        let now = now_secs();
        let enabled = {
            let net = &mut nets[idx];
            match net.nodes.iter_mut().find(|n| n.device_id == device.id) {
                Some(n) => {
                    n.name = device.name.clone();
                    n.lan_group = device.lan_group.clone();
                    n.ext_file_ep = device.ext_file_ep.clone();
                    n.platform = device.platform.clone();
                    n.online = true;
                    n.last_seen = now;
                    n.enabled
                }
                None => {
                    net.nodes.push(Node {
                        device_id: device.id.clone(),
                        name: device.name.clone(),
                        lan_group: device.lan_group.clone(),
                        ext_file_ep: device.ext_file_ep.clone(),
                        platform: device.platform.clone(),
                        enabled: false,
                        online: true,
                        last_seen: now,
                    });
                    false
                }
            }
        };
        let dev_id = device.id.clone();
        let status = if enabled { "active" } else { "pending" };
        let net = &nets[idx];
        let welcome = ServerToClient::Welcome {
            status: status.to_string(),
            network: NetworkInfo {
                id: net.id.clone(),
                name: net.name.clone(),
            },
            nodes: Self::enabled_node_infos(net),
        };
        drop(nets);
        let _ = tx.send(OutMsg::App(welcome));
        self.hub.register(&dev_id, &net_id, tx.clone());
        self.save().ok();
        Ok((net_id, dev_id))
    }

    pub fn touch(&self, dev_id: &str) {
        let mut nets = self.networks.lock().unwrap();
        for net in nets.iter_mut() {
            if let Some(n) = net.nodes.iter_mut().find(|n| n.device_id == dev_id) {
                n.last_seen = now_secs();
            }
        }
    }

    /// 文字中继门控：源与目标均须 enabled=true，且跨 lan_group（同 LAN 走直连不经服务端）。
    pub fn relay_text(
        &self,
        net_id: &str,
        from_dev: &str,
        to: &str,
        ct: &str,
        tx: &Tx,
    ) {
        let can_relay = {
            let nets = self.networks.lock().unwrap();
            let net = match nets.iter().find(|n| n.id == net_id) {
                Some(n) => n,
                None => return,
            };
            let from_node = match net.nodes.iter().find(|n| n.device_id == from_dev) {
                Some(n) => n,
                None => return,
            };
            if !from_node.enabled {
                let _ = tx.send(OutMsg::App(ServerToClient::Error {
                    code: "not_active".into(),
                    msg: "device not enabled".into(),
                }));
                return;
            }
            let to_node = match net.nodes.iter().find(|n| n.device_id == to) {
                Some(n) => n,
                None => {
                    let _ = tx.send(OutMsg::App(ServerToClient::Error {
                        code: "no_target".into(),
                        msg: "target not found".into(),
                    }));
                    return;
                }
            };
            if !to_node.enabled {
                let _ = tx.send(OutMsg::App(ServerToClient::Error {
                    code: "target_not_active".into(),
                    msg: "target not enabled".into(),
                }));
                return;
            }
            // 同 lan_group 设备应走直连，服务端不转发，避免重复投递
            to_node.lan_group != from_node.lan_group
        };
        if !can_relay {
            return;
        }
        self.hub.send(
            to,
            ServerToClient::RelayText {
                from: from_dev.to_string(),
                ct: ct.to_string(),
            },
        );
    }

    /// 文件通知：仅向其它已启用节点广播（manifest + 源 ext_file_ep），服务端不存字节。
    pub fn file_notify(
        &self,
        net_id: &str,
        from_dev: &str,
        manifest: serde_json::Value,
        ext_file_ep: &str,
        tx: &Tx,
    ) {
        {
            let nets = self.networks.lock().unwrap();
            let net = match nets.iter().find(|n| n.id == net_id) {
                Some(n) => n,
                None => return,
            };
            let from_node = match net.nodes.iter().find(|n| n.device_id == from_dev) {
                Some(n) => n,
                None => return,
            };
            if !from_node.enabled {
                let _ = tx.send(OutMsg::App(ServerToClient::Error {
                    code: "not_active".into(),
                    msg: "device not enabled".into(),
                }));
                return;
            }
        }
        let (targets, msg) = {
            let nets = self.networks.lock().unwrap();
            let net = match nets.iter().find(|n| n.id == net_id) {
                Some(n) => n,
                None => return,
            };
            let targets: Vec<String> = net
                .nodes
                .iter()
                .filter(|n| n.enabled && n.device_id != from_dev)
                .map(|n| n.device_id.clone())
                .collect();
            let msg = ServerToClient::FileNotify {
                from: from_dev.to_string(),
                manifest: manifest.clone(),
                ext_file_ep: ext_file_ep.to_string(),
            };
            (targets, msg)
        };
        for t in targets {
            self.hub.send(&t, msg.clone());
        }
    }

    /// 设备断开：标记 offline，广播 nodes_update（仅含 enabled 在线节点）。
    pub fn disconnect(&self, dev_id: &str) {
        self.hub.unregister(dev_id);
        {
            let mut nets = self.networks.lock().unwrap();
            for net in nets.iter_mut() {
                if let Some(n) = net.nodes.iter_mut().find(|n| n.device_id == dev_id) {
                    n.online = false;
                    n.last_seen = now_secs();
                }
            }
        }
        self.save().ok();
        let net_id = {
            let nets = self.networks.lock().unwrap();
            nets.iter()
                .find(|n| n.nodes.iter().any(|m| m.device_id == dev_id))
                .map(|n| n.id.clone())
        };
        if let Some(id) = net_id {
            self.broadcast_nodes_update(&id);
        }
    }

    pub fn broadcast_nodes_update(&self, net_id: &str) {
        let (dev_ids, msg) = {
            let nets = self.networks.lock().unwrap();
            let net = match nets.iter().find(|n| n.id == net_id) {
                Some(n) => n,
                None => return,
            };
            let dev_ids: Vec<String> = net
                .nodes
                .iter()
                .filter(|n| n.enabled && n.online)
                .map(|n| n.device_id.clone())
                .collect();
            let msg = ServerToClient::NodesUpdate {
                nodes: Self::enabled_node_infos(net),
            };
            (dev_ids, msg)
        };
        for d in dev_ids {
            self.hub.send(&d, msg.clone());
        }
    }

    pub fn enable_device(&self, net_id: &str, dev_id: &str) -> bool {
        {
            let mut nets = self.networks.lock().unwrap();
            let net = match nets.iter_mut().find(|n| n.id == net_id) {
                Some(n) => n,
                None => return false,
            };
            let node = match net.nodes.iter_mut().find(|n| n.device_id == dev_id) {
                Some(n) => n,
                None => return false,
            };
            node.enabled = true;
        }
        self.save().ok();
        self.hub.send(dev_id, ServerToClient::Activated);
        self.broadcast_nodes_update(net_id);
        true
    }

    pub fn disable_device(&self, net_id: &str, dev_id: &str) -> bool {
        {
            let mut nets = self.networks.lock().unwrap();
            let net = match nets.iter_mut().find(|n| n.id == net_id) {
                Some(n) => n,
                None => return false,
            };
            let node = match net.nodes.iter_mut().find(|n| n.device_id == dev_id) {
                Some(n) => n,
                None => return false,
            };
            node.enabled = false;
        }
        self.save().ok();
        self.hub.send(dev_id, ServerToClient::Deactivated);
        self.broadcast_nodes_update(net_id);
        true
    }
}
