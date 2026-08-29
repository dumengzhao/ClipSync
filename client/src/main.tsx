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

// [DEBUG] 挂载上报：确认 pull-toast 窗口实际渲染的是 PullToast 而不是 App。
// 若日志里出现 `窗口 pull-toast 挂载, is_toast=false`，说明 label 判断失效，
// 小窗会显示成主界面缩影 —— 表面「弹了」，用户却认不出那是待拉取小窗。
if (import.meta.env.DEV) {
  void import('@tauri-apps/api/core')
    .then((m) =>
      m.invoke('debug_report_mount', { label: getCurrentWindow().label, isToast }),
    )
    .catch(() => {});
}

// [DEBUG] 开发期便于在无对端时手动验证「待拉取小窗」：在 DevTools Console 输入
//   __simulateOffer()
// 即可走与真实对端完全相同的接收链路触发一次模拟 Offer（不会自动弹出，不会常驻）。
if (import.meta.env.DEV) {
  (window as unknown as { __simulateOffer?: () => Promise<unknown> }).__simulateOffer = () =>
    import('@tauri-apps/api/core').then((m) => m.invoke('simulate_incoming_offer'));
}
