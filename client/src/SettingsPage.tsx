import { useEffect, useState } from 'react';
import { getConfig, setConfig, quitApp, type AppConfig } from './api/tauri';

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

  const quit = () => {
    quitApp();
  };

  if (!cfg) return <div className="settings"><p>加载中…</p></div>;

  return (
    <div className="settings">
      <h1>设置</h1>

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
        <label>配对码 (6 位数字，两端需一致)</label>
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

      <div className="actions">
        <button className="btn" onClick={save}>
          保存
        </button>
        <button className="btn btn-ghost" onClick={onBack}>
          完成
        </button>
        <button className="btn btn-danger" onClick={quit}>
          退出 ClipSync
        </button>
      </div>
      {msg && <div className="msg">{msg}</div>}
    </div>
  );
}
