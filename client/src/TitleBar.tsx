import { useEffect, useRef, useState } from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getServerStatus, getConfig } from './api/tauri';

/**
 * 完全自定义窗口标题栏（decorations:false 时启用）。
 * - 左侧信息区显示跨局域网服务端连接状态（订阅 server-status 事件）；按钮区不触发拖动。
 * - 拖动用 OS 原生 startDragging（绝对跟手、无抖动）。
 * - 按钮 hover 底色用 JS 控制的 .is-hover 类（而非 CSS :hover），关闭点击时立即移除，
 *   窗口重开（main-shown）时再兜底清除，避免红/灰底残留与「闪一下」。
 */
export default function TitleBar() {
  // 跨局域网服务端连接状态：0 未连接 / 1 待启用 / 2 已启用
  const [serverStatus, setServerStatus] = useState(0);
  // 本机设备名称（设置中可改，挂载时回填一次）
  const [deviceName, setDeviceName] = useState('');
  // 重开瞬间抑制 hover：窗口出现时指针若停在按钮上会触发 mouseenter，
  // 必须忽略，直到用户真正移动鼠标，否则关闭按钮红底「闪一下」。
  const suppressHoverRef = useRef(false);

  useEffect(() => {
    // 挂载时回填初始连接状态（之后由 server-status 事件实时更新）
    getServerStatus().then(setServerStatus).catch(() => setServerStatus(0));
    // 挂载时回填本机设备名称（设置中可改，这里只取一次）
    getConfig().then((c) => setDeviceName(c.device_name)).catch(() => {});

    // 窗口再次显示时：
    // 1) 兜底清除残留 hover 底色；
    // 2) 抑制 hover 检测，直到用户真正移动鼠标——否则指针若停在关闭按钮上，
    //    WKWebView 在窗口出现时会派发 mouseenter，重新点亮红底 → 重开「闪一下」。
    const unlisten = listen('main-shown', () => {
      document
        .querySelectorAll('.titlebar .tb-btn.is-hover')
        .forEach((el) => el.classList.remove('is-hover'));
      suppressHoverRef.current = true;
      const body = document.body;
      const prev = body.style.pointerEvents;
      body.style.pointerEvents = 'none';
      void body.offsetHeight; // 强制重排
      body.style.pointerEvents = prev;
    });

    // 首次真实 mousemove 后解除抑制：此后正常 hover 高亮恢复
    const onFirstMove = () => {
      suppressHoverRef.current = false;
      window.removeEventListener('mousemove', onFirstMove);
    };
    window.addEventListener('mousemove', onFirstMove);

    // 跨局域网服务端连接状态：实时更新左侧状态文案
    const unlistenStatus = listen<number>('server-status', (e) =>
      setServerStatus(e.payload),
    );

    return () => {
      unlisten.then((u) => u());
      unlistenStatus.then((u) => u());
      window.removeEventListener('mousemove', onFirstMove);
    };
  }, []);

  // 最小化 / 最大化 / 拖动 全部走 Rust 命令（见 lib.rs 的 win_minimize / win_toggle_maximize /
  // win_set_position）——因为 JS 的 getCurrentWindow().minimize()/setPosition() 等写操作在本项目
  // 未被授予权限（日志曾报 allow-minimize / allow-set-position not allowed），Rust 侧调用无此限制。
  const minimize = () => {
    invoke('win_minimize').catch((e) => console.error('win_minimize failed', e));
  };

  const toggleMax = () => {
    invoke('win_toggle_maximize').catch((e) => console.error('win_toggle_maximize failed', e));
  };

  const close = (e: React.MouseEvent) => {
    // 关闭瞬间立即移除自身 hover 底色，避免窗口隐藏导致 mouseleave 不触发、
    // 重开时 main-shown 清除前先「闪一下」红底
    (e.currentTarget as HTMLElement).classList.remove('is-hover');
    invoke('hide_app_window').catch((e) => console.error('hide_app_window failed', e));
  };

  const setHover = (e: React.MouseEvent, on: boolean) => {
    if (suppressHoverRef.current && on) return; // 重开瞬间不点亮 hover 底色
    (e.currentTarget as HTMLElement).classList.toggle('is-hover', on);
  };

  // 原生拖动：交给 OS 接管鼠标捕获，绝对跟手、无抖动。
  // 旧实现「mousedown 记录坐标 + mousemove 调 setPosition 跟随」在 Windows 上会因 IPC 异步
  // 导致窗口更新滞后于鼠标而疯狂抖动。startDragging 由 OS 驱动（需 allow-start-dragging 权限）。
  // 必须在 mousedown 同步上下文内发起，不可先 await 其它 IPC。
  const onTitleMouseDown = (e: React.MouseEvent) => {
    if (e.button !== 0) return;
    if ((e.target as HTMLElement).closest('.tb-btn')) return; // 按钮区不拖动
    const win = getCurrentWindow();
    win.startDragging().catch((err) => console.error('startDragging failed', err));
  };

  return (
    <div className="titlebar" onMouseDown={onTitleMouseDown}>
      <div className="titlebar-info" title="跨局域网服务端连接状态">
        {deviceName && (
          <>
            <span className="titlebar-device">{deviceName}</span>
            <span className="titlebar-sep">·</span>
          </>
        )}
        <span
          className={
            'titlebar-status ' +
            (serverStatus === 2
              ? 'active'
              : serverStatus === 1
                ? 'pending'
                : 'disconnected')
          }
        >
          {serverStatus === 2
            ? '跨 LAN 同步 · 已启用'
            : serverStatus === 1
              ? '跨 LAN 同步 · 待启用'
              : '跨 LAN 同步 · 未连接'}
        </span>
      </div>
      <div className="titlebar-spacer" />
      <div className="titlebar-actions">
        <button
          className="tb-btn"
          onClick={minimize}
          onMouseEnter={(e) => setHover(e, true)}
          onMouseLeave={(e) => setHover(e, false)}
          title="最小化"
          aria-label="最小化"
        >
          <svg width="10" height="10" viewBox="0 0 10 10">
            <line x1="1.5" y1="5" x2="8.5" y2="5" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
          </svg>
        </button>
        <button
          className="tb-btn"
          onClick={toggleMax}
          onMouseEnter={(e) => setHover(e, true)}
          onMouseLeave={(e) => setHover(e, false)}
          title="最大化"
          aria-label="最大化"
        >
          <svg width="10" height="10" viewBox="0 0 10 10">
            <rect x="1.4" y="1.4" width="7.2" height="7.2" fill="none" stroke="currentColor" strokeWidth="1.1" />
          </svg>
        </button>
        <button
          className="tb-btn tb-close"
          onClick={(e) => close(e)}
          onMouseEnter={(e) => setHover(e, true)}
          onMouseLeave={(e) => setHover(e, false)}
          title="关闭"
          aria-label="关闭"
        >
          <svg width="10" height="10" viewBox="0 0 10 10">
            <line x1="2" y1="2" x2="8" y2="8" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
            <line x1="8" y1="2" x2="2" y2="8" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
          </svg>
        </button>
      </div>
    </div>
  );
}
