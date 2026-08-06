import { useEffect, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { getVersion } from './api/tauri';
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

  useEffect(() => {
    getVersion().then(setVersion).catch(() => setVersion('unknown'));
    // dev 下「托盘 → 设置」通过事件请求内嵌显示设置视图
    const unlisten = listen<null>('open-settings', () => setView('settings'));
    return () => {
      unlisten.then((u) => u());
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
        <p>跨平台剪贴板同步工具</p>
        <button className="btn" onClick={() => setView('settings')}>
          打开设置
        </button>
        <p className="hint">开发中，详见 docs/development-plan.md</p>
      </main>
    </div>
  );
}
