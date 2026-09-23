/// <reference types="vite/client" />
import React from 'react';
import ReactDOM from 'react-dom/client';
import App from './App';
import PullToast from './PullToast';
import LogWindow from './LogWindow';
import { getCurrentWindow } from '@tauri-apps/api/window';
import './styles.css';

const winLabel = getCurrentWindow().label;
/**
 * 窗口 label 常量：与 Rust 侧保持一致。
 * - `log-viewer` ↔ `src-tauri/src/log_viewer.rs` 的 `LOG_WINDOW_LABEL`
 * - `pull-toast` ↔ `src-tauri/tauri.conf.json` 的静态窗口声明
 * Tauri 没有跨语言共享机制，**改动必须两处同步**（另见 capabilities/default.json 的 windows 白名单）。
 */
const isToast = winLabel === 'pull-toast';
const isLogViewer = winLabel === 'log-viewer';

ReactDOM.createRoot(document.getElementById('root') as HTMLElement).render(
  <React.StrictMode>
    {isToast ? <PullToast /> : isLogViewer ? <LogWindow /> : <App />}
  </React.StrictMode>,
);

/**
 * 全局错误上报 —— release 也生效（后端 frontend_log 会带上窗口 label 写进日志文件）。
 *
 * 没有这段时，界面上出现的异常（小窗里的报错文案、渲染崩溃、未处理的 Promise 拒绝）
 * 在日志里完全查不到：2026-09-22 排查「弹窗里有错误」时就卡在这里 —— 用户看得见、我们查不到。
 */
function reportFrontendError(kind: string, detail: unknown): void {
  const text =
    detail instanceof Error ? detail.name + ': ' + detail.message : String(detail);
  void import('@tauri-apps/api/core')
    .then((m) => m.invoke('frontend_log', { msg: kind + ': ' + text }))
    .catch(() => {});
}

window.addEventListener('error', (e) => reportFrontendError('window.error', e.error ?? e.message));
window.addEventListener('unhandledrejection', (e) =>
  reportFrontendError('unhandledrejection', e.reason),
);

// [DEBUG] 挂载上报（仅 dev）：确认 pull-toast 窗口渲染的是 PullToast 而不是 App ——
// 若 label 判断失效，小窗会显示成主界面缩影，表面「弹了」但用户认不出。
if (import.meta.env.DEV && isToast) {
  void import('@tauri-apps/api/core')
    .then((m) => m.invoke('frontend_log', { msg: 'pull-toast 已挂载（label 判断正常）' }))
    .catch(() => {});
}

// [DEBUG] 开发期便于在无对端时手动验证「待拉取小窗」。DevTools Console 输入：
//   __simulateOffer()      → 模拟局域网内对端发文件（本地 P2P 路径）
//   __simulateCrossLan()   → 模拟跨 LAN 对端发文件（跨 LAN 路径）
// 两者都走与真实链路相同的代码，不会自动弹出、不会常驻。
if (import.meta.env.DEV) {
  const w = window as unknown as {
    __simulateOffer?: () => Promise<unknown>;
    __simulateCrossLan?: () => Promise<unknown>;
  };
  const core = () => import('@tauri-apps/api/core');
  w.__simulateOffer = () => core().then((m) => m.invoke('simulate_incoming_offer'));
  w.__simulateCrossLan = () => core().then((m) => m.invoke('simulate_cross_lan_offer'));
}
