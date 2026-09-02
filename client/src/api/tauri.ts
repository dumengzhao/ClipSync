import { invoke } from '@tauri-apps/api/core';

export async function getVersion(): Promise<string> {
  return invoke<string>('get_version');
}

export async function getDeviceId(): Promise<string> {
  return invoke<string>('get_device_id');
}

export async function getPairedDevices(): Promise<PairedDeviceInfo[]> {
  return invoke<PairedDeviceInfo[]>('get_paired_devices');
}

/** 已配对设备信息（与 Rust 端 `DeviceInfo` 一致） */
export interface PairedDeviceInfo {
  id: string;
  name: string;
  fingerprint: string;
  trusted: boolean;
  last_seen: number;
  /** 对端最后一次出现的可拨号地址（host:port） */
  last_addr?: string | null;
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

/** 手动发起配对：作为发起方用对方显示的配对码连接指定对端（局域网发现列表内） */
export async function pairWith(deviceId: string, code: string): Promise<void> {
  return invoke<void>('pair_with', { deviceId, code });
}

/** 通过手动地址（跨网络 / mDNS 被拦截）发起首配对，用对方显示的配对码作为 SPAKE2 口令 */
export async function pairManual(addr: string, port: number, code: string): Promise<void> {
  return invoke<void>('pair_manual', { addr, port, code });
}

/** 重新生成本机配对码并立即持久化，返回新码 */
export async function regeneratePairingCode(): Promise<string> {
  return invoke<string>('regenerate_pairing_code');
}

/** 取消与某设备的配对：清除持久化口令与设备记录并断开连接 */
export async function unpair(deviceId: string): Promise<void> {
  return invoke<void>('unpair', { deviceId });
}

/** 单个文件条目（文件清单中用） */
export interface FileItem {
  file_name: string;
  file_size: number;
  is_dir: boolean;
  relative_path: string;
}

/** 对端广播的「待拉取」文件传输 */
export interface PendingOffer {
  transfer_id: string;
  device_id: string;
  device_name: string;
  files: FileItem[];
  total_size: number;
  /** 是否满足自动拉取阈值（由对端在 Offer 时一并广播） */
  auto_pull?: boolean;
  /** 顶层条目名（文件夹名或文件名），用于折叠显示 */
  top_names?: string[];
  /** 顶层是否含文件夹（文件夹传输） */
  has_folder?: boolean;
}

/** 拉取某次文件传输：下载到本机 sync_dir，完成后自动写剪贴板 */
export async function pullFiles(transferId: string): Promise<void> {
  return invoke<void>('pull_files', { transferId });
}

/** 查询当前待拉取清单（挂载时回填，兜底事件丢失） */
export async function listPendingOffers(): Promise<PendingOffer[]> {
  return invoke<PendingOffer[]>('list_pending_offers');
}

/** 读取本机剪贴板当前文字内容；剪贴板无文字或读取失败时返回 null */
export async function getClipboardText(): Promise<string | null> {
  try {
    return await invoke<string>('get_clipboard');
  } catch {
    return null;
  }
}

/** 应用配置（与 Rust 端 `AppConfig` 字段保持一致） */
export interface AppConfig {
  device_name: string;
  auto_start: boolean;
  /** 开机自启后是否显示主窗口（仅 auto_start 为真时生效）；默认 true */
  show_main_window_on_launch?: boolean;
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
  /** 文件同步落盘目录；为空时回退系统下载目录 */
  sync_dir?: string | null;
  /** 是否开启自动拉取：关闭后任何文件都需手动点「拉取」；此开关优先于阈值。默认 true */
  auto_pull_enabled?: boolean;
  /** 自动拉取阈值（MB）：仅当 auto_pull_enabled 开启、且对端文件总大小小于此值时才自动拉取。默认 1 */
  auto_pull_threshold_mb?: number;
  /** 复制文件夹时递归文件数上限：超过此值则拦截推送、仅本地提示请压缩。0 表示不限制。默认 100 */
  max_folder_files?: number;
  // ===== 跨局域网中转（服务端） =====
  /** 服务端 WebSocket 地址，例如 ws://your-host:20070/ws；为空表示不使用服务端 */
  server_url?: string;
  /** Network Token（共享密钥）：服务端鉴权 + 跨 LAN 文字端到端加密 */
  network_token?: string;
  /** 本机对外文件拉取地址 ip:port（公网可达）；为空则跨 LAN 文件不可拉取 */
  ext_file_ep?: string;
  /** 局域网分组标识：相同值视为同一局域网（走直连），不同值走服务端；为空按网段自动推断 */
  lan_group?: string;
  /** 待拉取小窗「未操作自动关闭」时长（毫秒）。未点击「拉取」则此时长后自动收起；
   *  一旦点击拉取，改为等拉取完成并写入本机剪贴板后再关闭。0 表示不自动关闭。默认 15000 */
  toast_auto_hide_ms?: number;
  /** 主窗口默认宽度（逻辑像素）；不设置（null/undefined）时采用应用默认尺寸。需与 window_height 同时设置才生效 */
  window_width?: number | null;
  /** 主窗口默认高度（逻辑像素）；不设置（null/undefined）时采用应用默认尺寸。需与 window_width 同时设置才生效 */
  window_height?: number | null;
}

export async function getConfig(): Promise<AppConfig> {
  return invoke<AppConfig>('get_config');
}

export async function setConfig(cfg: AppConfig): Promise<void> {
  return invoke<void>('set_config', { cfg });
}

/** 获取主窗口当前实际尺寸（逻辑像素），用于「获取实时宽高」回填 */
export async function getWindowSize(): Promise<{ width: number; height: number }> {
  return invoke<{ width: number; height: number }>('get_window_size');
}

export async function openSettings(): Promise<void> {
  return invoke<void>('open_settings');
}

export async function quitApp(): Promise<void> {
  return invoke<void>('quit_app');
}

// ===== 跨局域网中转（服务端） =====

/** 服务端连接状态：0 未连接 / 1 待审批(pending) / 2 已启用(active) */
export async function getServerStatus(): Promise<number> {
  return invoke<number>('get_server_status');
}

/** 跨局域网已启用节点（来自服务端下发） */
export interface RemoteNode {
  device_id: string;
  name: string;
  lan_group: string;
  ext_file_ep: string;
  platform: string;
}
export async function getServerNodes(): Promise<RemoteNode[]> {
  return invoke<RemoteNode[]>('get_server_nodes');
}

/** 跨 LAN「待复制」文件通知 */
export interface CrossLanOffer {
  from: string;
  from_name: string;
  manifest: { file_name: string; file_size: number; is_dir: boolean }[];
  ext_file_ep: string;
}
export async function listCrossLanOffers(): Promise<CrossLanOffer[]> {
  return invoke<CrossLanOffer[]>('list_cross_lan_offers');
}

/** 拉取某条跨 LAN 文件通知（从对端 ext_file_ep 下载并写本机剪贴板） */
export async function pullCrossLan(
  pullId: string,
  extFileEp: string,
  manifest: unknown,
): Promise<void> {
  return invoke<void>('pull_cross_lan', {
    pullId,
    extFileEp,
    manifest,
  });
}

/**
 * 跨 LAN 待拉取条目的稳定 base id（不含 `local:` 前缀）。
 * 必须与 PullToast 用于 item.id 的 `crossItemId` 同源——后端据此把进度事件
 * (transfer_id = 此 base) 经 `local:` 前缀拼接后精确投递到对应小窗条目。
 * 不能只用 from+ext_file_ep（同一设备连续发多文件会误判重复，见 PullToast 注释）。
 */
export function crossItemBase(o: CrossLanOffer): string {
  const names = (o.manifest || [])
    .map((f: { file_name: string }) => f.file_name)
    .join('、');
  const total = (o.manifest || []).reduce(
    (s: number, f: { file_size: number }) => s + (f.file_size || 0),
    0,
  );
  return `${o.from}:${o.ext_file_ep}:${names}:${total}`;
}

/** 跨 LAN 条目在小窗中的完整 id（带 `local:` 前缀，与进度事件 key 对齐）。 */
export function crossItemId(o: CrossLanOffer): string {
  return `local:${crossItemBase(o)}`;
}

