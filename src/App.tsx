import { useEffect, useState } from 'react';
import { getVersion } from './api/tauri';

function App() {
  const [version, setVersion] = useState<string>('');

  useEffect(() => {
    getVersion().then(setVersion).catch(() => setVersion('unknown'));
  }, []);

  return (
    <div className="app">
      <header className="app-header">
        <h1>ClipSync</h1>
        <p className="version">v{version}</p>
      </header>
      <main className="app-main">
        <p>跨平台剪贴板同步工具</p>
        <p className="hint">开发中，详见 docs/development-plan.md</p>
      </main>
    </div>
  );
}

export default App;
