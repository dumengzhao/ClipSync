/// <reference types="vite/client" />
import React from 'react';
import ReactDOM from 'react-dom/client';
import App from './App';
import PullToast from './PullToast';
import { getCurrentWindow } from '@tauri-apps/api/window';
import './styles.css';

const isToast = getCurrentWindow().label === 'pull-toast';

ReactDOM.createRoot(document.getElementById('root') as HTMLElement).render(
  <React.StrictMode>
    {isToast ? <PullToast /> : <App />}
  </React.StrictMode>,
);

// [DEBUG] 开发期便于在无对端时手动验证「待拉取小窗」：在 DevTools Console 输入
//   __simulateOffer()
// 即可走与真实对端完全相同的接收链路触发一次模拟 Offer（不会自动弹出，不会常驻）。
if (import.meta.env.DEV) {
  (window as unknown as { __simulateOffer?: () => Promise<unknown> }).__simulateOffer = () =>
    import('@tauri-apps/api/core').then((m) => m.invoke('simulate_incoming_offer'));
}
