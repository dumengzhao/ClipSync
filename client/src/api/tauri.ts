import { invoke } from '@tauri-apps/api/core';

export async function getVersion(): Promise<string> {
  return invoke<string>('get_version');
}

export async function getDeviceId(): Promise<string> {
  return invoke<string>('get_device_id');
}

export async function getPairedDevices(): Promise<unknown[]> {
  return invoke<unknown[]>('get_paired_devices');
}

/** 通过 mDNS 发现的局域网对端（与 Rust 端 `DiscoveredPeer` 一致） */
export interface DiscoveredPeer {
  device_id: string;
  device_name: string;
  addr: string;
  port: number;
}

/** 已建立加密连接的对端（peer-connected 事件负载） */
export interface ConnectedPeer {
  device_id: string;
  device_name: string;
  addr: string;
}

/** 列出当前已发现的局域网设备 */
export async function listDiscoveredPeers(): Promise<DiscoveredPeer[]> {
  return invoke<DiscoveredPeer[]>('list_discovered_peers');
}

/** 列出当前已建立加密通道的对端 device_id（用于挂载时回填在线状态） */
export async function listConnectedPeers(): Promise<string[]> {
  return invoke<string[]>('list_connected_peers');
}

/** 应用配置（与 Rust 端 `AppConfig` 字段保持一致） */
export interface AppConfig {
  device_name: string;
  auto_start: boolean;
  sync_text: boolean;
  sync_image: boolean;
  sync_file: boolean;
  max_file_size_mb: number;
  max_image_size_mb: number;
  listen_port: number;
  enable_mdns: boolean;
  pairing_code: string;
  manual_addresses: { label: string; addr: string; port: number }[];
  sync_primary_selection: boolean;
  cache_ttl_hours: number;
  theme: 'System' | 'Light' | 'Dark';
}

export async function getConfig(): Promise<AppConfig> {
  return invoke<AppConfig>('get_config');
}

export async function setConfig(cfg: AppConfig): Promise<void> {
  return invoke<void>('set_config', { cfg });
}

export async function openSettings(): Promise<void> {
  return invoke<void>('open_settings');
}

export async function quitApp(): Promise<void> {
  return invoke<void>('quit_app');
}
