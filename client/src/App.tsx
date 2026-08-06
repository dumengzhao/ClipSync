import { useEffect, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import {
  getVersion,
  getPairedDevices,
  listDiscoveredPeers,
  listConnectedPeers,
  generatePairingCode,
  cancelPairing,
  getPendingPairing,
  pairWith,
  unpair,
  listPendingOffers,
  pullFiles,
  type DiscoveredPeer,
  type PairedDeviceInfo,
  type PendingOffer,
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
  const [discovered, setDiscovered] = useState<DiscoveredPeer[]>([]);
  const [paired, setPaired] = useState<PairedDeviceInfo[]>([]);
  const [connected, setConnected] = useState<Set<string>>(new Set());
  // 当前武装中的配对码（生成配对码后展示，配对成功或取消后清除）
  const [pendingCode, setPendingCode] = useState<string | null>(null);
  // 正在输入配对码的目标设备（点击「配对」后进入输入态）
  const [pairingTarget, setPairingTarget] = useState<DiscoveredPeer | null>(null);
  const [pairInput, setPairInput] = useState('');
  const [msg, setMsg] = useState('');
  // 「待拉取」文件清单（对端拷贝后广播过来，本端显示，用户点「拉取」才下载）
  const [pendingOffers, setPendingOffers] = useState<PendingOffer[]>([]);
  // 正在拉取中的传输 ID 集合（拉取中禁用按钮、显示「拉取中…」）
  const [pulling, setPulling] = useState<Set<string>>(new Set());
  // 已完成的拉取结果：transfer_id -> 落盘目录与文件数，供用户在主页查看下载位置
  const [pullResults, setPullResults] = useState<
    Record<string, { device_name: string; target_dir: string; file_count: number }>
  >({});

  const flash = (m: string) => {
    setMsg(m);
    window.setTimeout(() => setMsg(''), 4000);
  };

  useEffect(() => {
    getVersion().then(setVersion).catch(() => setVersion('unknown'));
    // dev 下「托盘 → 设置」通过事件请求内嵌显示设置视图
    const unlistenSettings = listen<null>('open-settings', () => setView('settings'));

    // 初始数据：已发现设备 / 已配对设备 / 在线状态 / 武装中的配对码
    listDiscoveredPeers().then(setDiscovered).catch(() => {});
    getPairedDevices().then(setPaired).catch(() => {});
    listConnectedPeers()
      .then((ids) => setConnected(new Set(ids)))
      .catch(() => {});
    getPendingPairing().then(setPendingCode).catch(() => {});
    // 待拉取清单：挂载时主动查一次（兜底事件丢失），并实时订阅对端广播
    listPendingOffers().then(setPendingOffers).catch(() => {});

    // 对端拷贝文件后广播「待拉取」；本端点击拉取后收到开始/完成事件
    const unlistenFileOffer = listen<PendingOffer>('file-offer', (e) => {
      setPendingOffers((prev) => {
        if (prev.some((o) => o.transfer_id === e.payload.transfer_id)) return prev;
        return [...prev, e.payload];
      });
    });
    const unlistenPullStart = listen<{ transfer_id: string }>(
      'file-pull-start',
      (e) => {
        setPulling((prev) => new Set(prev).add(e.payload.transfer_id));
      },
    );
    const unlistenPullComplete = listen<{
      transfer_id: string;
      device_name: string;
      target_dir: string;
      file_count: number;
    }>('file-pull-complete', (e) => {
      setPulling((prev) => {
        const n = new Set(prev);
        n.delete(e.payload.transfer_id);
        return n;
      });
      setPullResults((prev) => ({
        ...prev,
        [e.payload.transfer_id]: {
          device_name: e.payload.device_name,
          target_dir: e.payload.target_dir,
          file_count: e.payload.file_count,
        },
      }));
      flash(`已从「${e.payload.device_name}」拉取 ${e.payload.file_count} 个文件`);
    });

    // 实时订阅局域网发现 / 配对 / 连接状态
    const unlistenDiscovered = listen<DiscoveredPeer>('peer-discovered', (e) => {
      setDiscovered((prev) => {
        if (prev.some((p) => p.device_id === e.payload.device_id)) {
          return prev.map((p) => (p.device_id === e.payload.device_id ? e.payload : p));
        }
        return [...prev, e.payload];
      });
    });
    const unlistenLost = listen<string>('peer-lost', (e) => {
      setDiscovered((prev) => prev.filter((p) => p.device_id !== e.payload));
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
    const unlistenPaired = listen<PairedDeviceInfo>('peer-paired', (e) => {
      setPaired((prev) => {
        if (prev.some((p) => p.id === e.payload.id)) {
          return prev.map((p) => (p.id === e.payload.id ? e.payload : p));
        }
        return [...prev, e.payload];
      });
      // 配对成功，清除武装码展示
      setPendingCode(null);
      setPairingTarget(null);
      flash(`已与「${e.payload.name}」配对`);
    });
    const unlistenFailed = listen<{ device_id: string; reason: string }>(
      'pairing-failed',
      (e) => flash(`配对失败：${e.payload.reason}`),
    );
    const unlistenUnpaired = listen<string>('peer-unpaired', (e) => {
      setPaired((prev) => prev.filter((p) => p.id !== e.payload));
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
      unlistenPaired.then((u) => u());
      unlistenFailed.then((u) => u());
      unlistenUnpaired.then((u) => u());
      unlistenFileOffer.then((u) => u());
      unlistenPullStart.then((u) => u());
      unlistenPullComplete.then((u) => u());
    };
  }, []);

  const generateCode = async () => {
    try {
      const code = await generatePairingCode();
      setPendingCode(code);
    } catch (e) {
      flash('生成配对码失败: ' + String(e));
    }
  };

  const cancelCode = async () => {
    try {
      await cancelPairing();
    } catch {
      /* ignore */
    }
    setPendingCode(null);
  };

  const submitPair = async () => {
    if (!pairingTarget) return;
    const code = pairInput.trim();
    if (code.length === 0) {
      flash('请输入配对码');
      return;
    }
    const target = pairingTarget;
    setPairingTarget(null);
    setPairInput('');
    try {
      await pairWith(target.device_id, code);
      flash(`正在与「${target.device_name}」配对…`);
    } catch (e) {
      flash('配对发起失败: ' + String(e));
    }
  };

  const removePairing = async (p: PairedDeviceInfo) => {
    try {
      await unpair(p.id);
      flash(`已取消与「${p.name}」的配对`);
    } catch (e) {
      flash('取消配对失败: ' + String(e));
    }
  };

  /// 把字节数格式化为可读字符串（B / KB / MB / GB）
  const fmtSize = (n: number): string => {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
    return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
  };

  /// 点击「拉取」：向对端请求文件，本端下载到 sync_dir，完成后自动写本机剪贴板。
  const doPull = async (transferId: string) => {
    setPulling((prev) => new Set(prev).add(transferId));
    try {
      await pullFiles(transferId);
      // 后端收到请求后会把该 transfer 从待拉取清单移除，这里同步清掉前端展示
      setPendingOffers((prev) => prev.filter((o) => o.transfer_id !== transferId));
    } catch (e) {
      setPulling((prev) => {
        const n = new Set(prev);
        n.delete(transferId);
        return n;
      });
      flash('拉取失败: ' + String(e));
    }
  };

  const pairedIds = new Set(paired.map((p) => p.id));
  const discoveredIds = new Set(discovered.map((p) => p.device_id));
  // 已配对设备不重复出现在「发现」列表中
  const discoveredOnly = discovered.filter((p) => !pairedIds.has(p.device_id));

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
        {pendingCode && (
          <div className="pairing-banner">
            <div className="pairing-banner-title">配对码（请告知对方，让其在「配对」时输入）</div>
            <div className="pairing-code">{pendingCode}</div>
            <button className="btn btn-ghost btn-sm" onClick={cancelCode}>
              取消
            </button>
          </div>
        )}

        <section className="peers">
          <h2>已配对设备</h2>
          {paired.length === 0 ? (
            <p className="hint">还没有已配对设备，请在下方「局域网发现的设备」中发起配对</p>
          ) : (
            <ul className="peer-list">
              {paired.map((p) => {
                const isConnected = connected.has(p.id);
                // 已被发现却还没连上，说明后台正在重连——如实告知，避免误以为卡死
                const status = isConnected
                  ? '已连接'
                  : discoveredIds.has(p.id)
                    ? '连接中…'
                    : '离线';
                return (
                  <li key={p.id} className="peer-item peer-item-action">
                    <span className={`peer-dot ${isConnected ? 'on' : 'off'}`} />
                    <span className="peer-name">{p.name}</span>
                    <span className="peer-addr">
                      {status}
                      {p.fingerprint ? ` · ${p.fingerprint.slice(0, 8)}` : ''}
                    </span>
                    <button className="btn btn-ghost btn-sm" onClick={() => removePairing(p)}>
                      取消配对
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </section>

        <section className="peers">
          <h2>局域网发现的设备</h2>
          {discoveredOnly.length === 0 ? (
            <p className="hint">局域网内未发现其它 ClipSync 设备</p>
          ) : (
            <ul className="peer-list">
              {discoveredOnly.map((p) => {
                const isPairing = pairingTarget?.device_id === p.device_id;
                return (
                  <li key={p.device_id} className="peer-item peer-item-action">
                    <span className="peer-name">{p.device_name}</span>
                    <span className="peer-addr">
                      {p.addr}:{p.port}
                    </span>
                    {isPairing ? (
                      <span className="pair-input-row">
                        <input
                          className="pair-input"
                          autoFocus
                          placeholder="输入对方配对码"
                          value={pairInput}
                          onChange={(e) => setPairInput(e.target.value)}
                          onKeyDown={(e) => {
                            if (e.key === 'Enter') submitPair();
                          }}
                        />
                        <button className="btn btn-sm" onClick={submitPair}>
                          确认
                        </button>
                        <button
                          className="btn btn-ghost btn-sm"
                          onClick={() => {
                            setPairingTarget(null);
                            setPairInput('');
                          }}
                        >
                          取消
                        </button>
                      </span>
                    ) : (
                      <button className="btn btn-sm" onClick={() => setPairingTarget(p)}>
                        配对
                      </button>
                    )}
                  </li>
                );
              })}
            </ul>
          )}
          <button className="btn btn-ghost" onClick={generateCode}>
            生成配对码
          </button>
        </section>

        <section className="peers">
          <h2>待拉取文件</h2>
          {pendingOffers.length === 0 ? (
            <p className="hint">还没有待拉取的文件。当对端拷贝文件/图片后，会出现在这里</p>
          ) : (
            <ul className="peer-list">
              {pendingOffers.map((o) => {
                const isPulling = pulling.has(o.transfer_id);
                return (
                  <li key={o.transfer_id} className="peer-item peer-item-action">
                    <span className="peer-name">{o.device_name || o.device_id}</span>
                    <span className="peer-addr">
                      {o.files.length} 个文件 · {fmtSize(o.total_size)}
                    </span>
                    {isPulling ? (
                      <span className="peer-addr">拉取中…</span>
                    ) : (
                      <button className="btn btn-sm" onClick={() => doPull(o.transfer_id)}>
                        拉取
                      </button>
                    )}
                  </li>
                );
              })}
            </ul>
          )}
        </section>

        {Object.keys(pullResults).length > 0 && (
          <section className="peers">
            <h2>已拉取的文件</h2>
            <ul className="peer-list">
              {Object.entries(pullResults).map(([tid, r]) => (
                <li key={tid} className="peer-item">
                  <span className="peer-name">{r.device_name}</span>
                  <span className="peer-addr">
                    已保存 {r.file_count} 个文件到：{r.target_dir}
                  </span>
                </li>
              ))}
            </ul>
            <p className="hint">完成后文件已自动写入本机剪贴板，直接 Ctrl+V 即可粘贴到目标位置</p>
          </section>
        )}

        {msg && <div className="msg">{msg}</div>}

        <button className="btn" onClick={() => setView('settings')}>
          打开设置
        </button>
        <p className="hint">
          局域网剪贴板同步（mDNS 自动发现 + 强制交互式 SPAKE2 配对）。一方「生成配对码」，
          另一方在「局域网发现的设备」中点击「配对」并输入该码即可建立加密通道。
          配对只需一次——之后重启会自动重连。
        </p>
      </main>
    </div>
  );
}
