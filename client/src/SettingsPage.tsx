import { useEffect, useRef, useState, type KeyboardEvent, type FocusEvent } from 'react';
import {
  getConfig,
  setConfig,
  getWindowSize,
  pairManual,
  regeneratePairingCode,
  checkUpdate,
  downloadUpdate,
  installUpdate,
  type AppConfig,
  type UpdateInfo,
} from './api/tauri';
import { open } from '@tauri-apps/plugin-dialog';
import { applyTheme } from './theme';

// 校验文本是否为合法 IPv4 地址（四段、每段 0-255、仅数字与点）
function isIpv4(s: string): boolean {
  const parts = s.split('.');
  if (parts.length !== 4) return false;
  return parts.every((o) => /^\d{1,3}$/.test(o) && Number(o) <= 255);
}

/**
 * 拆分服务端地址：把完整 `ws(s)://host:port/ws` 拆成「协议 + 主机:端口」两部分。
 * 兼容用户直接粘贴完整 URL 或省略协议/路径的写法。
 */
function parseServerUrl(url: string): { scheme: 'ws' | 'wss'; host: string } {
  const s = (url ?? '').trim();
  let scheme: 'ws' | 'wss' = 'ws';
  let rest = s;
  const m = /^([a-zA-Z][a-zA-Z0-9+.-]*):\/\//.exec(s);
  if (m) {
    if (m[1].toLowerCase() === 'wss') scheme = 'wss';
    rest = s.slice(m[0].length);
  }
  const slash = rest.indexOf('/'); // 去掉路径（含固定的 /ws）
  if (slash >= 0) rest = rest.slice(0, slash);
  return { scheme, host: rest };
}

/** 拼装服务端地址：协议下拉 + 主机:端口 输入框 + 固定 /ws 路径。主机为空则不设置。 */
function buildServerUrl(scheme: 'ws' | 'wss', host: string): string {
  const cleaned = parseServerUrl(host).host;
  return cleaned ? `${scheme}://${cleaned}/ws` : '';
}

/**
 * 设置窗口页面。
 *
 * 复用 `AppConfig` 单一配置结构；后续新增配置项只需：
 *   1) 在 Rust 端 `AppConfig` 增加字段 + 默认值
 *   2) 在下方表单增加一个对应的 row
 * 窗口创建、hash 路由、命令通道均无需改动——天然多端统一、可扩展。
 */
export default function SettingsPage({ onBack }: { onBack: () => void }) {
  const [cfg, setCfg] = useState<AppConfig | null>(null);
  // 以「弹出式 toast」替代底部静态文字，确保保存结果等反馈一定可见（底部 msg 容易看不到）。
  const [toast, setToast] = useState<{ text: string; type: 'ok' | 'err' } | null>(null);
  const toastTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const setMsg = (text: string, forceType?: 'ok' | 'err') => {
    const type: 'ok' | 'err' =
      forceType ?? (/失败|错误|无效|不正确|未授权/.test(text) ? 'err' : 'ok');
    if (toastTimer.current) clearTimeout(toastTimer.current);
    setToast({ text, type });
    toastTimer.current = setTimeout(() => setToast(null), 1800);
  };
  const [thresholdWarn, setThresholdWarn] = useState('');
  // 「默认窗口宽高」直接读 config（启动时若未设置已由 Rust 端写入当前窗口尺寸作为默认值），
  // 不再在打开设置时动态获取，故拖动改变窗口大小不会影响显示值；仅手动输入或点「获取实时宽高」才改。
  // 服务端地址拆分为：ws/wss 协议下拉 + 主机:端口 输入框（/ws 路径固定，不显示、不可编辑）
  const [srvScheme, setSrvScheme] = useState<'ws' | 'wss'>('ws');
  const [srvHost, setSrvHost] = useState('');
  // 手动连接地址（mDNS 被防火墙拦截时的兜底直连）
  const [maLabel, setMaLabel] = useState('');
  const [maAddr, setMaAddr] = useState('');
  const [maPort, setMaPort] = useState('');
  // 手动地址配对时输入的对方配对码（按当前正在配对的那条地址记录）
  const [manualPairing, setManualPairing] = useState<{ addr: string; port: number } | null>(null);
  const [manualPairCode, setManualPairCode] = useState('');
  // 记录已落盘的快照，用于判断「值是否真的变化」，避免受控组件重渲染导致的误判。
  const persistedRef = useRef<AppConfig | null>(null);
  // 客户端自更新（基址取自 server_url 配置，无签名自托管）
  const [upd, setUpd] = useState<UpdateInfo | null>(null);
  const [updBusy, setUpdBusy] = useState<'check' | 'download' | null>(null);
  const [updMsg, setUpdMsg] = useState('');

  useEffect(() => {
    getConfig()
      .then((c) => {
        setCfg(c);
        persistedRef.current = c;
        const p = parseServerUrl(c.server_url ?? '');
        setSrvScheme(p.scheme);
        setSrvHost(p.host);
        applyTheme(c.theme);
      })
      .catch((e) => setMsg('加载配置失败: ' + String(e)));
    // 不再于打开设置时动态获取窗口尺寸：默认值已在启动时由 Rust 端写入 config。
  }, []);

  // 「检查更新」：调 Rust 自写更新器；null=已是最新/未发布
  const doCheckUpdate = async () => {
    setUpdBusy('check');
    setUpdMsg('');
    try {
      const r = await checkUpdate();
      if (!r) {
        setUpd(null);
        setUpdMsg('已是最新');
      } else {
        setUpd(r);
        setUpdMsg('');
      }
    } catch (e) {
      setUpd(null);
      setUpdMsg('检查失败: ' + String(e));
    } finally {
      setUpdBusy(null);
    }
  };

  // 「下载并安装」：下载 + sha256 校验 + 拉起安装包（Windows 上随后自动退出旧进程）
  const doInstall = async () => {
    if (!upd) return;
    setUpdBusy('download');
    setUpdMsg('');
    try {
      const p = await downloadUpdate(upd.url, upd.sha256);
      setUpdMsg('校验通过，正在启动安装…');
      await installUpdate(p);
    } catch (e) {
      setUpdMsg('安装失败: ' + String(e));
    } finally {
      setUpdBusy(null);
    }
  };

  const update = <K extends keyof AppConfig>(key: K, value: AppConfig[K]) => {
    setCfg((c) => (c ? { ...c, [key]: value } : c));
  };

  // 配置未加载完成时先渲染占位，避免下方函数内 cfg 被判为可能为 null
  if (!cfg) return <div className="settings"><p>加载中…</p></div>;

  // 直接持久化：把增量 patch 写回后端并弹提示，无需统一的保存按钮。
  const persist = async (patch: Partial<AppConfig>, okMsg = '已保存') => {
    if (!cfg) return;
    const next: AppConfig = { ...cfg, ...patch };
    setCfg(next);
    persistedRef.current = next;
    try {
      await setConfig(next);
      if ('theme' in patch && patch.theme) applyTheme(patch.theme);
      setMsg(okMsg);
    } catch (e) {
      setMsg('保存失败: ' + String(e));
    }
  };

  // 「获取实时宽高」：取当前主窗口实际尺寸，回填并立即保存；不影响手动输入宽高的独立保存。
  const handleGetRealtimeSize = async () => {
    try {
      const s = await getWindowSize();
      update('window_width', s.width);
      update('window_height', s.height);
      await persist({ window_width: s.width, window_height: s.height }, '已获取并保存当前窗口宽高');
    } catch (e) {
      setMsg('获取窗口尺寸失败: ' + String(e), 'err');
    }
  };

  // 宽/高输入框落盘：仅当值真正发生变化才写盘。特别地，配置本为「未设置」且本次值恰好等于
  // 「当前窗口实时尺寸」(仅作为显示回退) 时，视为未改动，不写盘，避免把「未设置」误存成「已设置」。
  const commitSize = (key: 'window_width' | 'window_height', raw: string) => {
    const trimmed = raw.trim();
    const n = Math.round(Number(trimmed));
    const v: number | null = trimmed === '' || !Number.isFinite(n) ? null : Math.max(0, n);
    const cur = persistedRef.current
      ? (persistedRef.current[key] as number | null | undefined)
      : undefined;
    if (v === cur) return;
    persist({ [key]: v } as Partial<AppConfig>);
  };

  // 文本框：回车或失焦即保存（带类型转换，且值未变时不写盘）。
  const textSave = (
    key: keyof AppConfig,
    parse: (s: string) => AppConfig[keyof AppConfig] = (s) => s,
  ) => {
    const commit = (raw: string) => {
      const v = parse(raw);
      const cur = persistedRef.current ? persistedRef.current[key] : undefined;
      if (v === cur) return; // 无变化不写盘
      persist({ [key]: v } as Partial<AppConfig>);
    };
    return {
      onKeyDown: (e: KeyboardEvent<HTMLInputElement>) => {
        if (e.key === 'Enter') {
          commit((e.target as HTMLInputElement).value);
          (e.target as HTMLInputElement).blur(); // 保存后让输入框失焦
        }
      },
      onBlur: (e: FocusEvent<HTMLInputElement>) => {
        commit((e.target as HTMLInputElement).value);
      },
    };
  };

  // 对外文件地址仅允许 IPv4：校验并在不合法时阻止落盘，错误统一走 toast 提示
  const isValidExtEp = (v: string): boolean => {
    if (v === '') return true; // 空 = 不设置，合法
    return isIpv4(v);
  };
  const commitExtEp = () => {
    const v = cfg.ext_file_ep ?? '';
    if (!isValidExtEp(v)) {
      // 不合法：阻止落盘，并通过统一的 toast 弹出错误提示（显式 err 红框，避免文案误判为成功）
      setMsg('请输入有效的 IPv4 地址（如 1.2.3.4）', 'err');
      return;
    }
    const cur = persistedRef.current?.ext_file_ep ?? '';
    if (v === cur) return; // 无变化不写盘
    persist({ ext_file_ep: v });
  };

  // 服务端地址：失焦即把「协议 + 主机:端口」拼回完整 ws(s)://host:port/ws 落盘
  const commitServerUrl = () => {
    const next = buildServerUrl(srvScheme, srvHost);
    if ((cfg.server_url ?? '') === next) return; // 无变化不写盘
    persist({ server_url: next });
  };

  const pickSyncDir = async () => {
    const selected = await open({ directory: true, multiple: false });
    if (typeof selected === 'string') {
      update('sync_dir', selected);
    }
  };

  const addManual = () => {
    if (!cfg) return;
    const addr = maAddr.trim();
    const port = Number(maPort);
    if (!addr || !Number.isFinite(port) || port <= 0 || port > 65535) {
      setMsg('请填写有效的地址和端口（端口范围 1–65535）');
      return;
    }
    const label = maLabel.trim() || `${addr}:${port}`;
    const list = cfg.manual_addresses ? [...cfg.manual_addresses] : [];
    if (list.some((m) => m.label === label)) {
      setMsg('该标签的手动地址已存在');
      return;
    }
    list.push({ label, addr, port });
    update('manual_addresses', list);
    setMaLabel('');
    setMaAddr('');
    setMaPort('');
    persist({ manual_addresses: list }, '已添加（已保存，配对设备将自动尝试直连）');
  };

  const removeManual = (i: number) => {
    if (!cfg) return;
    const list = (cfg.manual_addresses ?? []).filter((_, idx) => idx !== i);
    update('manual_addresses', list);
    persist({ manual_addresses: list }, '已删除该手动地址（已保存）');
  };

  // 通过手动地址配对时，输入「对方界面显示的配对码」作为 SPAKE2 口令发起首配对。
  // 两端配对码各自独立、无需预先相同。
  const startManualPair = async (addr: string, port: number) => {
    const code = manualPairCode.trim();
    if (!code) {
      setMsg('请先输入对方界面显示的配对码');
      return;
    }
    try {
      await pairManual(addr, port, code);
      setMsg(`正在与「${addr}:${port}」配对…（请输入对方界面显示的配对码）`);
      setManualPairing(null);
      setManualPairCode('');
    } catch (e) {
      setMsg('配对发起失败: ' + String(e));
    }
  };

  // 重新生成「配对码」：新码即刻生效，已配对设备不受影响（它们走 link secret 重连）。
  const refreshPairingCode = async () => {
    try {
      const code = await regeneratePairingCode();
      update('pairing_code', code);
      setMsg('已刷新配对码（已配对设备不受影响）');
    } catch (e) {
      setMsg('刷新失败: ' + String(e));
    }
  };

  return (
    <div className="settings">
      <div className="settings-header">
        <div className="settings-header-inner">
          <div style={{ display: 'flex', alignItems: 'center', gap: '0.75rem' }}>
            <button className="btn btn-ghost btn-sm" onClick={onBack}>
              返回
            </button>
            <h1>设置</h1>
          </div>
        </div>
      </div>

      <div className="settings-body">

      <div className="section">设备</div>
      <div className="row">
        <label>设备名称</label>
        <input
          type="text"
          value={cfg.device_name}
          onChange={(e) => update('device_name', e.target.value)}
          {...textSave('device_name')}
        />
      </div>

      <div className="section">网络</div>
      <div className="row">
        <label>监听端口（默认 20071，可改）</label>
        <input
          type="number"
          value={cfg.listen_port}
          onChange={(e) => update('listen_port', Number(e.target.value))}
          {...textSave('listen_port', (s) => Number(s))}
        />
      </div>
      <div className="row">
        <label>启用局域网发现 (mDNS)</label>
        <input
          type="checkbox"
          checked={cfg.enable_mdns}
          onChange={(e) => persist({ enable_mdns: e.target.checked })}
        />
      </div>

      <div className="section">启动</div>
      <div className="row">
        <label>开机自启</label>
        <input
          type="checkbox"
          checked={cfg.auto_start}
          onChange={(e) => persist({ auto_start: e.target.checked })}
        />
      </div>
      <div className="row">
        <label>启动后是否打开主窗口</label>
        <input
          type="checkbox"
          checked={cfg.show_main_window_on_launch ?? true}
          onChange={(e) => persist({ show_main_window_on_launch: e.target.checked })}
        />
      </div>

      <div className="row">
        <label>配对码</label>
        <div style={{ display: 'flex', gap: '0.5rem', flex: '0 0 auto' }}>
          <input
            type="text"
            style={{ width: '160px' }}
            value={cfg.pairing_code}
            onChange={(e) => update('pairing_code', e.target.value)}
          />
          <button className="btn btn-sm btn-ghost" onClick={refreshPairingCode}>
            刷新
          </button>
        </div>
      </div>

      <div className="section">同步内容</div>
      <div className="row">
        <label>同步文本</label>
        <input type="checkbox" checked={cfg.sync_text} onChange={(e) => persist({ sync_text: e.target.checked })} />
      </div>
      <div className="row">
        <label>同步图片</label>
        <input type="checkbox" checked={cfg.sync_image} onChange={(e) => persist({ sync_image: e.target.checked })} />
      </div>
      <div className="row">
        <label>同步文件</label>
        <input type="checkbox" checked={cfg.sync_file} onChange={(e) => persist({ sync_file: e.target.checked })} />
      </div>
      <div className="row">
        <label>同步主选区 (Linux)</label>
        <input
          type="checkbox"
          checked={cfg.sync_primary_selection}
          onChange={(e) => persist({ sync_primary_selection: e.target.checked })}
        />
      </div>
      <div className="row">
        <label>最大图片大小 (MB)</label>
        <input
          type="number"
          value={cfg.max_image_size_mb}
          onChange={(e) => update('max_image_size_mb', Number(e.target.value))}
          {...textSave('max_image_size_mb', (s) => Number(s))}
        />
      </div>
      <div className="row">
        <label>最大文件大小 (MB)</label>
        <input
          type="number"
          value={cfg.max_file_size_mb}
          onChange={(e) => update('max_file_size_mb', Number(e.target.value))}
          {...textSave('max_file_size_mb', (s) => Number(s))}
        />
      </div>

      <div className="section">文件同步</div>
      <div className="row">
        <label>文件同步目录（留空则用系统下载目录）</label>
        <div style={{ display: 'flex', gap: '0.5rem', flex: '0 0 auto' }}>
          <input
            type="text"
            style={{ width: '180px' }}
            placeholder="例如 D:/ClipSync"
            value={cfg.sync_dir ?? ''}
            onChange={(e) => update('sync_dir', e.target.value)}
          />
          <button className="btn btn-sm btn-ghost" onClick={pickSyncDir}>
            浏览
          </button>
        </div>
      </div>
      <div className="row">
        <label>自动拉取（关闭后需手动点「拉取」）</label>
        <input
          type="checkbox"
          checked={cfg.auto_pull_enabled ?? false}
          onChange={(e) => update('auto_pull_enabled', e.target.checked)}
        />
      </div>
      <div className="row">
        <label>自动拉取阈值 (MB，默认 1；超过 10 将提醒)</label>
        <input
          type="number"
          min={0}
          value={cfg.auto_pull_threshold_mb ?? 1}
          onChange={(e) => {
            const raw = e.target.value.trim();
            const v = Math.round(Number(raw));
            if (!Number.isFinite(v) || v < 0) return;
            update('auto_pull_threshold_mb', v as number);
            if (v > 10) {
              setThresholdWarn(
                '阈值超过 10MB：小文件将自动拉取到本机，可能占用较多带宽与磁盘空间（仍可保存）。',
              );
            } else {
              setThresholdWarn('');
            }
          }}
          {...textSave('auto_pull_threshold_mb', (s) => {
            const v = Math.round(Number(s));
            return Number.isFinite(v) && v >= 0 ? v : 1;
          })}
        />
      </div>
      {thresholdWarn && (
        <p className="hint" style={{ color: '#d97706' }}>
          {thresholdWarn}
        </p>
      )}
      <div className="row">
        <label>文件夹文件数上限（0 不限制，默认 100）</label>
        <input
          type="number"
          min={0}
          value={cfg.max_folder_files ?? 100}
          onChange={(e) => {
            const raw = e.target.value.trim();
            const v = Math.round(Number(raw));
            if (!Number.isFinite(v) || v < 0) return;
            update('max_folder_files', v as number);
          }}
          {...textSave('max_folder_files', (s) => {
            const v = Math.round(Number(s));
            return Number.isFinite(v) && v >= 0 ? v : 0;
          })}
        />
      </div>

      <div className="section">手动连接地址（mDNS 被拦截时的兜底直连）</div>
      {cfg.manual_addresses && cfg.manual_addresses.length > 0 && (
        <ul className="peer-list">
          {cfg.manual_addresses.map((m, i) => (
            <li
              key={m.label}
              className="peer-item"
              style={{ flexDirection: 'column', alignItems: 'stretch' }}
            >
              <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem' }}>
                <span className="peer-name">{m.label}</span>
                <span className="peer-addr">
                  {m.addr}:{m.port}
                </span>
                <button
                  className="btn btn-sm"
                  onClick={() => {
                    setManualPairing({ addr: m.addr, port: m.port });
                    setManualPairCode('');
                  }}
                >
                  配对
                </button>
                <button className="btn btn-sm btn-ghost" onClick={() => removeManual(i)}>
                  删除
                </button>
              </div>
              {manualPairing && manualPairing.addr === m.addr && manualPairing.port === m.port && (
                <div style={{ display: 'flex', gap: '0.5rem', marginTop: '0.4rem' }}>
                  <input
                    type="text"
                    style={{ flex: 1 }}
                    autoFocus
                    placeholder="输入对方显示的配对码"
                    value={manualPairCode}
                    onChange={(e) => setManualPairCode(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter') startManualPair(m.addr, m.port);
                    }}
                  />
                  <button className="btn btn-sm" onClick={() => startManualPair(m.addr, m.port)}>
                    确认
                  </button>
                  <button
                    className="btn btn-sm btn-ghost"
                    onClick={() => {
                      setManualPairing(null);
                      setManualPairCode('');
                    }}
                  >
                    取消
                  </button>
                </div>
              )}
            </li>
          ))}
        </ul>
      )}
      <div className="row" style={{ gap: '0.5rem' }}>
        <input type="text" style={{ flex: '0 0 120px' }} placeholder="标签（可选）" value={maLabel} onChange={(e) => setMaLabel(e.target.value)} />
        <input type="text" style={{ flex: '1 1 auto' }} placeholder="地址 / IP" value={maAddr} onChange={(e) => setMaAddr(e.target.value)} />
        <input
          type="number"
          style={{ flex: '0 0 70px' }}
          placeholder="端口"
          value={maPort}
          onChange={(e) => setMaPort(e.target.value)}
        />
        <button className="btn btn-sm" onClick={addManual}>
          添加
        </button>
      </div>
      <p className="hint">
        已配对设备只要曾经连上过，本端就会记住对方地址并在断线后自动重连；此处再填对方地址，
        可在防火墙拦截局域网发现（mDNS）时兜底直连。
      </p>

      <div className="section">跨局域网中转（服务端）</div>
      <div className="row">
        <label>服务端地址</label>
        <div style={{ display: 'flex', alignItems: 'center', gap: '0.4rem', flex: '0 0 auto' }}>
          <select
            style={{ width: 'auto', padding: '0.35rem 0.4rem' }}
            value={srvScheme}
            onChange={(e) => {
              // 切换 ws/wss 立即生效：拼回完整地址落盘并触发重连
              const sc = e.target.value as 'ws' | 'wss';
              setSrvScheme(sc);
              const next = buildServerUrl(sc, srvHost);
              if ((cfg.server_url ?? '') !== next) persist({ server_url: next });
            }}
          >
            <option value="ws">ws://</option>
            <option value="wss">wss://</option>
          </select>
          <input
            type="text"
            style={{ width: '200px' }}
            placeholder="例如 clipsync.example.com:20070"
            value={srvHost}
            onChange={(e) => setSrvHost(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') (e.target as HTMLInputElement).blur();
            }}
            onBlur={() => commitServerUrl()}
          />
          <span className="ext-ep-port">/ws</span>
        </div>
      </div>
      <div className="row">
        <label>Network Token（共享密钥）</label>
        <input
          type="text"
          style={{ width: '260px' }}
          placeholder="服务端创建网络时返回的一次性 Token"
          value={cfg.network_token ?? ''}
          onChange={(e) => update('network_token', e.target.value)}
          {...textSave('network_token')}
        />
      </div>
      <div className="row">
        <label>对外文件地址</label>
        <div style={{ display: 'flex', alignItems: 'center', gap: '0.4rem', flex: '0 0 auto' }}>
          <input
            type="text"
            style={{ width: '200px' }}
            placeholder="例如 1.2.3.4"
            value={cfg.ext_file_ep ?? ''}
            onChange={(e) => {
              // 输入框只允许 IPv4 字符（数字与点），端口/字母/空格等一律剔除
              const ip = e.target.value.replace(/[^0-9.]/g, '');
              update('ext_file_ep', ip);
            }}
            onKeyDown={(e) => {
              if (e.key === 'Enter') (e.target as HTMLInputElement).blur();
            }}
            onBlur={() => commitExtEp()}
          />
          <span className="ext-ep-port">:{cfg.listen_port}</span>
        </div>
      </div>
      <p className="hint">
        输入框只填本机对外可达的 IPv4 地址，右侧端口自动取监听端口，无需填写；
        对端将按「ip:{cfg.listen_port}」拉取文件。
      </p>
      <div className="row">
        <label>局域网分组 (lanGroup，可选)</label>
        <input
          type="text"
          style={{ width: '160px' }}
          placeholder="留空按网段自动推断"
          value={cfg.lan_group ?? ''}
          onChange={(e) => update('lan_group', e.target.value)}
          {...textSave('lan_group')}
        />
      </div>
      <p className="hint">
        填入并失焦/回车即自动重连服务端（无需重启）：本机常连服务端，实现跨局域网文字/文件同步。
        设备需在服务端管理页被管理员「启用」后才参与同步。
      </p>

      <div className="section">缓存</div>
      <div className="row">
        <label>文件缓存有效期 (小时)</label>
        <input
          type="number"
          value={cfg.cache_ttl_hours}
          onChange={(e) => update('cache_ttl_hours', Number(e.target.value))}
          {...textSave('cache_ttl_hours', (s) => Number(s))}
        />
      </div>

      <div className="section">待拉取小窗</div>
      <div className="row">
        <label>未操作自动关闭 (秒)</label>
        <input
          type="number"
          min={0}
          value={Math.round((cfg.toast_auto_hide_ms ?? 15000) / 1000)}
          onChange={(e) =>
            update('toast_auto_hide_ms', Math.max(0, Number(e.target.value)) * 1000)
          }
          {...textSave('toast_auto_hide_ms', (s) => Math.max(0, Number(s)) * 1000)}
        />
      </div>
      <p className="hint">
        小窗弹出后，若在该时长内<b>未点击「拉取」</b>则自动关闭；一旦点击了拉取，就取消倒计时，
        改为<b>等拉取完成并写入本机剪贴板之后</b>才关闭（拉取中绝不会自动关闭）。
        填 0 表示从不自动关闭，需手动处理。
      </p>

      <div className="section">主窗口</div>
      <div className="row">
        <label style={{ display: 'inline-flex', alignItems: 'center' }}>
          默认窗口宽高
          <span
            onClick={handleGetRealtimeSize}
            style={{ color: '#3b82f6', cursor: 'pointer', marginLeft: '0.6rem', fontSize: '0.85em' }}
          >
            获取实时宽高
          </span>
        </label>
        <div style={{ display: 'flex', alignItems: 'center', gap: '0.4rem', flex: '0 0 auto' }}>
          <input
            type="number"
            min={1}
            style={{ width: '100px' }}
            placeholder="宽"
            value={cfg.window_width ?? ''}
            onChange={(e) => {
              const raw = e.target.value.trim();
              const n = Math.round(Number(raw));
              update('window_width', raw === '' || !Number.isFinite(n) ? null : Math.max(0, n));
            }}
            onKeyDown={(e) => {
              if (e.key === 'Enter') (e.target as HTMLInputElement).blur();
            }}
            onBlur={(e) => commitSize('window_width', e.target.value)}
          />
          <span>×</span>
          <input
            type="number"
            min={1}
            style={{ width: '100px' }}
            placeholder="高"
            value={cfg.window_height ?? ''}
            onChange={(e) => {
              const raw = e.target.value.trim();
              const n = Math.round(Number(raw));
              update('window_height', raw === '' || !Number.isFinite(n) ? null : Math.max(0, n));
            }}
            onKeyDown={(e) => {
              if (e.key === 'Enter') (e.target as HTMLInputElement).blur();
            }}
            onBlur={(e) => commitSize('window_height', e.target.value)}
          />
        </div>
      </div>
      <p className="hint">
        默认值已在启动时写入当前窗口尺寸；如需固定为特定值请填写宽与高（需<b>同时填写</b>）。点击「获取实时宽高」可填入并保存当前窗口实际尺寸。
      </p>

      <div className="section">外观</div>
      <div className="row">
        <label>主题</label>
        <select
          value={cfg.theme}
          onChange={(e) => persist({ theme: e.target.value as AppConfig['theme'] })}
        >
          <option value="System">跟随系统</option>
          <option value="Light">浅色</option>
          <option value="Dark">深色</option>
        </select>
      </div>

      <div className="section">更新</div>
      <div className="row">
        <label>检查新版本（更新地址取自服务端配置）</label>
        <button onClick={doCheckUpdate} disabled={updBusy !== null}>
          {updBusy === 'check' ? '检查中…' : '检查更新'}
        </button>
        {upd && (
          <button onClick={doInstall} disabled={updBusy !== null}>
            {updBusy === 'download' ? '下载中…' : `下载并安装 v${upd.version}`}
          </button>
        )}
      </div>
      {(upd || updMsg) && (
        <p className="hint">
          {upd
            ? `发现 v${upd.version}${upd.pub_date ? `（${upd.pub_date}）` : ''}${
                upd.notes ? `：${upd.notes}` : ''
              }`
            : updMsg}
        </p>
      )}

      {toast && (
        <div className={`toast toast-${toast.type}`} role="status">
          {toast.text}
        </div>
      )}
      </div>
    </div>
  );
}
