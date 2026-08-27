import { useEffect, useState, type MouseEvent } from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

/**
 * 完全自定义窗口标题栏（decorations:false 时启用）。
 * - 左侧为可拖动区 + 自定义信息（应用名 · 本机设备名）；
 * - 右侧三个统一风格的按钮：最小化 / 最大化 / 关闭。
 * 拖动用 Window.startDragging()（标题栏 mousedown 时触发，按钮区除外），
 * 不再依赖 data-tauri-drag-region / -webkit-app-region，避免与按钮点击冲突。
 * 关闭走 hide_app_window 命令，复用 Rust「隐藏而非退出」逻辑。
 */
export default function TitleBar() {
  const [deviceName, setDeviceName] = useState('');

  useEffect(() => {
    invoke<string>('get_device_name')
      .then(setDeviceName)
      .catch(() => setDeviceName(''));

    // 窗口再次显示时强制回流，清除隐藏期间残留的 :hover 红底（指针未真正移开导致卡住）
    const unlisten = listen('main-shown', () => {
      const body = document.body;
      const prev = body.style.pointerEvents;
      body.style.pointerEvents = 'none';
      void body.offsetHeight; // 强制重排，让浏览器重算 hover 状态
      body.style.pointerEvents = prev;
    });
    return () => {
      unlisten.then((u) => u());
    };
  }, []);

  // 标题栏 mousedown 触发拖动；点到按钮则跳过（按钮各自处理点击）
  const onTitleMouseDown = (e: MouseEvent) => {
    const target = e.target as HTMLElement;
    if (target.closest('.tb-btn')) return;
    if (e.buttons === 1) {
      getCurrentWindow().startDragging().catch(() => {});
    }
  };

  const minimize = () => {
    getCurrentWindow().minimize().catch(() => {});
  };

  const toggleMax = () => {
    getCurrentWindow().toggleMaximize().catch(() => {});
  };

  const close = () => {
    // 复用 Rust 的「关闭=隐藏」语义，而不是直接 destroy
    invoke('hide_app_window').catch(() => {});
  };

  return (
    <div className="titlebar" onMouseDown={onTitleMouseDown}>
      <div className="titlebar-info">
        <span className="titlebar-logo">ClipSync</span>
        {deviceName && <span className="titlebar-sep">·</span>}
        {deviceName && <span className="titlebar-device">{deviceName}</span>}
      </div>
      <div className="titlebar-actions">
        <button className="tb-btn" onClick={minimize} title="最小化" aria-label="最小化">
          <svg width="10" height="10" viewBox="0 0 10 10">
            <line x1="1.5" y1="5" x2="8.5" y2="5" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
          </svg>
        </button>
        <button className="tb-btn" onClick={toggleMax} title="最大化" aria-label="最大化">
          <svg width="10" height="10" viewBox="0 0 10 10">
            <rect x="1.4" y="1.4" width="7.2" height="7.2" fill="none" stroke="currentColor" strokeWidth="1.1" />
          </svg>
        </button>
        <button className="tb-btn tb-close" onClick={close} title="关闭" aria-label="关闭">
          <svg width="10" height="10" viewBox="0 0 10 10">
            <line x1="2" y1="2" x2="8" y2="8" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
            <line x1="8" y1="2" x2="2" y2="8" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
          </svg>
        </button>
      </div>
    </div>
  );
}
