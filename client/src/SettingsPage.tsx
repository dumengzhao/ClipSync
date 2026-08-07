import { useEffect, useState } from 'react';
import { getConfig, setConfig, type AppConfig } from './api/tauri';
import { open } from '@tauri-apps/plugin-dialog';

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
  const [msg, setMsg] = useState('');
  const [thresholdWarn, setThresholdWarn] = useState('');
  // 手动连接地址（mDNS 被防火墙拦截时的兜底直连）
  const [maLabel, setMaLabel] = useState('');
  const [maAddr, setMaAddr] = useState('');
  const [maPort, setMaPort] = useState('');

  useEffect(() => {
    getConfig()
      .then(setCfg)
      .catch((e) => setMsg('加载配置失败: ' + String(e)));
  }, []);

  const update = <K extends keyof AppConfig>(key: K, value: AppConfig[K]) => {
    setCfg((c) => (c ? { ...c, [key]: value } : c));
  };

  const save = async () => {
    if (!cfg) return;
    try {
      await setConfig(cfg);
      setMsg('已保存（端口等部分设置即时或重启后生效）');
    } catch (e) {
      setMsg('保存失败: ' + String(e));
    }
  };

  const pickSyncDir = async () => {
    const selected = await open({ directory: true, multiple: false });
    if (typeof selected === 'string') {
      update('sync_dir', selected);
    }
  };

  const addManual = () => {
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
    setMsg('已添加，点「保存」后生效（配对设备将自动尝试直连）');
  };

  const removeManual = (i: number) => {
    const list = (cfg.manual_addresses ?? []).filter((_, idx) => idx !== i);
    update('manual_addresses', list);
  };

  if (!cfg) return <div className="settings"><p>加载中…</p></div>;

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
          <button className="btn btn-sm" onClick={save}>
            保存
          </button>
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
        />
      </div>

      <div className="section">网络</div>
      <div className="row">
        <label>监听端口（默认 24681，可改）</label>
        <input
          type="number"
          value={cfg.listen_port}
          onChange={(e) => update('listen_port', Number(e.target.value))}
        />
      </div>
      <div className="row">
        <label>启用局域网发现 (mDNS)</label>
        <input
          type="checkbox"
          checked={cfg.enable_mdns}
          onChange={(e) => update('enable_mdns', e.target.checked)}
        />
      </div>
      <div className="row">
        <label>预留配对码（交互式配对暂不使用）</label>
        <input
          type="text"
          value={cfg.pairing_code}
          onChange={(e) => update('pairing_code', e.target.value)}
        />
      </div>

      <div className="section">同步内容</div>
      <div className="row">
        <label>同步文本</label>
        <input type="checkbox" checked={cfg.sync_text} onChange={(e) => update('sync_text', e.target.checked)} />
      </div>
      <div className="row">
        <label>同步图片</label>
        <input type="checkbox" checked={cfg.sync_image} onChange={(e) => update('sync_image', e.target.checked)} />
      </div>
      <div className="row">
        <label>同步文件</label>
        <input type="checkbox" checked={cfg.sync_file} onChange={(e) => update('sync_file', e.target.checked)} />
      </div>
      <div className="row">
        <label>同步主选区 (Linux)</label>
        <input
          type="checkbox"
          checked={cfg.sync_primary_selection}
          onChange={(e) => update('sync_primary_selection', e.target.checked)}
        />
      </div>
      <div className="row">
        <label>最大图片大小 (MB)</label>
        <input
          type="number"
          value={cfg.max_image_size_mb}
          onChange={(e) => update('max_image_size_mb', Number(e.target.value))}
        />
      </div>
      <div className="row">
        <label>最大文件大小 (MB)</label>
        <input
          type="number"
          value={cfg.max_file_size_mb}
          onChange={(e) => update('max_file_size_mb', Number(e.target.value))}
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
        />
      </div>
      {thresholdWarn && (
        <p className="hint" style={{ color: '#d97706' }}>
          {thresholdWarn}
        </p>
      )}

      <div className="section">手动连接地址（mDNS 被拦截时的兜底直连）</div>
      {cfg.manual_addresses && cfg.manual_addresses.length > 0 && (
        <ul className="peer-list">
          {cfg.manual_addresses.map((m, i) => (
            <li key={m.label} className="peer-item">
              <span className="peer-name">{m.label}</span>
              <span className="peer-addr">
                {m.addr}:{m.port}
              </span>
              <button className="btn btn-sm btn-ghost" onClick={() => removeManual(i)}>
                删除
              </button>
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

      <div className="section">缓存</div>
      <div className="row">
        <label>文件缓存有效期 (小时)</label>
        <input
          type="number"
          value={cfg.cache_ttl_hours}
          onChange={(e) => update('cache_ttl_hours', Number(e.target.value))}
        />
      </div>

      <div className="section">外观</div>
      <div className="row">
        <label>主题</label>
        <select value={cfg.theme} onChange={(e) => update('theme', e.target.value as AppConfig['theme'])}>
          <option value="System">跟随系统</option>
          <option value="Light">浅色</option>
          <option value="Dark">深色</option>
        </select>
      </div>

      {msg && <div className="msg">{msg}</div>}
      </div>
    </div>
  );
}
