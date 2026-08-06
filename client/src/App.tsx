import { useEffect, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import {
  getVersion,
  listDiscoveredPeers,
  listConnectedPeers,
  type DiscoveredPeer,
} from './api/tauri';
import SettingsPage from './SettingsPage';

/**
 * 主窗口。设置视图在主窗口内嵌显示（dev 模式下 Tauri 额外窗口加载前端不可靠，
 * 会白屏；主窗口自身渲染稳定），因此 dev 下点「打开设置」或托盘「设置」都切换到
 * 内嵌的设置视图，避免白屏且保证可关闭。独立设置窗口的创建逻辑保留在 Rust 端，
 * 供正式 build 版本使用。
 */
export default function App() {
  const [version, setVersion] = useState('');
  const [view, setView] = useState<'main' | 'settings'>('main');
  const [peers, setPeers] = useState<DiscoveredPeer[]>([]);
  const [connected, setConnected] = useState<Set<string>>(new Set());

  useEffect(() => {
    getVersion().then(setVersion).catch(() => setVersion('unknown'));
    // dev 下「托盘 → 设置」通过事件请求内嵌显示设置视图
    const unlistenSettings = listen<null>('open-settings', () => setView('settings'));

    // 拉取初始已发现设备
    listDiscoveredPeers().then(setPeers).catch(() => {});

    // 主动查询当前已连接对端，兜底 peer-connected 事件可能早于本监听就绪而丢失的竞态
    listConnectedPeers()
      .then((ids) => setConnected(new Set(ids)))
      .catch(() => {});

    // 实时订阅局域网发现 / 连接状态
    const unlistenDiscovered = listen<DiscoveredPeer>('peer-discovered', (e) => {
      setPeers((prev) => {
        // 按 device_id 覆盖更新（对端改名/换端口后重新广播时刷新），
        // 不存在才追加，避免重复条目
        if (prev.some((p) => p.device_id === e.payload.device_id)) {
          return prev.map((p) => (p.device_id === e.payload.device_id ? e.payload : p));
        }
        return [...prev, e.payload];
      });
    });
    const unlistenLost = listen<string>('peer-lost', (e) => {
      setPeers((prev) => prev.filter((p) => p.device_id !== e.payload));
      setConnected((prev) => {
        const n = new Set(prev);
        n.delete(e.payload);
        return n;
      });
    });
    const unlistenConnected = listen<{ device_id: string }>('peer-connected', (e) => {
      setConnected((prev) => new Set(prev).add(e.payload.device_id));
    });
    const unlistenDisconnected = listen<string>('peer-disconnected', (e) => {
      setConnected((prev) => {
        const n = new Set(prev);
        n.delete(e.payload);
        return n;
      });
    });

    return () => {
      unlistenSettings.then((u) => u());
      unlistenDiscovered.then((u) => u());
      unlistenLost.then((u) => u());
      unlistenConnected.then((u) => u());
      unlistenDisconnected.then((u) => u());
    };
  }, []);

  if (view === 'settings') {
    return <SettingsPage onBack={() => setView('main')} />;
  }

  return (
    <div className="app">
      <header className="app-header">
        <h1>ClipSync</h1>
        <p className="version">v{version}</p>
      </header>
      <main className="app-main">
        <section className="peers">
          <h2>发现的设备</h2>
          {peers.length === 0 ? (
            <p className="hint">局域网内未发现其它 ClipSync 设备</p>
          ) : (
            <ul className="peer-list">
              {peers.map((p) => {
                const isConnected = connected.has(p.device_id);
                return (
                  <li key={p.device_id} className="peer-item">
                    <span className={`peer-dot ${isConnected ? 'on' : 'off'}`} />
                    <span className="peer-name">{p.device_name}</span>
                    <span className="peer-addr">
                      {p.addr}:{p.port}
                      {isConnected ? ' · 已连接' : ''}
                    </span>
                  </li>
                );
              })}
            </ul>
          )}
        </section>
        <button className="btn" onClick={() => setView('settings')}>
          打开设置
        </button>
        <p className="hint">局域网剪贴板同步已启用（mDNS 自动发现 + SPAKE2 加密配对）</p>
      </main>
    </div>
  );
}
