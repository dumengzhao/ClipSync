import { useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { getConfig } from './api/tauri';
import { applyTheme } from './theme';

/**
 * 实时日志窗口（label = "log-viewer"）。
 *
 * 数据源：Rust 侧文件 tail 任务（生命周期与窗口一致）——
 * - `log-line`：批量新行（string[]）
 * - `log-reset`：日志文件轮转/重建，清空当前视图
 *
 * 功能（用户选定）：自动滚底 + 暂停/恢复滚动 + 复制全部 + 清屏。
 * 「不留存」：关闭窗口即销毁，无任何持久化；重新打开由 Rust 重放尾部历史。
 */

/** 视图中保留的最大行数（超出丢弃最旧的，防长时间运行内存膨胀） */
const MAX_VIEW_LINES = 5000;
/** 渲染节流：批量 append 的时间窗口（毫秒） */
const FLUSH_MS = 120;

export default function LogWindow() {
  const [lines, setLines] = useState<string[]>([]);
  const [paused, setPaused] = useState(false);
  const [copied, setCopied] = useState(false);
  const listRef = useRef<HTMLDivElement>(null);
  // 暂存待渲染的行：暂停期间只入队不渲染，恢复时一次性补上（不丢日志）
  const pendingRef = useRef<string[]>([]);
  const flushTimerRef = useRef<number | null>(null);
  // 暂停状态镜像供事件回调读最新值（监听只挂载一次，闭包会捕获首帧值）
  const pausedRef = useRef(paused);
  pausedRef.current = paused;
  // 是否自动跟随底部：用户上滚查看历史时暂停跟随，滚回底部恢复
  const followRef = useRef(true);

  useEffect(() => {
    // 与主窗口保持一致的主题（日志窗口是独立 WebView，需自行应用一次）
    getConfig()
      .then((c) => applyTheme(c.theme))
      .catch(() => {});

    const unlisten = listen<string[]>('log-line', (e) => {
      const batch = e.payload;
      if (!Array.isArray(batch) || batch.length === 0) return;
      const pending = pendingRef.current;
      pending.push(...batch);
      // 上限保护：极端情况下不让 pending 无限增长
      if (pending.length > MAX_VIEW_LINES) {
        pending.splice(0, pending.length - MAX_VIEW_LINES);
      }
      if (pausedRef.current) return; // 暂停：只入队
      if (flushTimerRef.current !== null) return; // 已在节流窗口内
      flushTimerRef.current = window.setTimeout(() => {
        flushTimerRef.current = null;
        flushPending();
      }, FLUSH_MS);
    });

    const unreset = listen('log-reset', () => {
      pendingRef.current = [];
      setLines([]);
    });

    return () => {
      void unlisten.then((f) => f());
      void unreset.then((f) => f());
      if (flushTimerRef.current !== null) window.clearTimeout(flushTimerRef.current);
    };
  }, []);

  /** 把 pending 批量并入视图（按 MAX_VIEW_LINES 截断） */
  const flushPending = () => {
    const incoming = pendingRef.current;
    pendingRef.current = [];
    if (incoming.length === 0) return;
    setLines((prev) => {
      const next = prev.concat(incoming);
      return next.length > MAX_VIEW_LINES ? next.slice(next.length - MAX_VIEW_LINES) : next;
    });
  };

  // 自动滚底：跟随状态且未暂停时贴住底部
  useEffect(() => {
    if (paused || !followRef.current) return;
    const el = listRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [lines, paused]);

  /** 用户滚动意图：离开底部即停止跟随，回到接近底部则恢复 */
  const onScroll = () => {
    const el = listRef.current;
    if (!el) return;
    followRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
  };

  const togglePause = () => {
    setPaused((p) => {
      const next = !p;
      if (!next) {
        // 恢复：把暂停期间积累的行立刻补进视图
        window.setTimeout(() => {
          followRef.current = true;
          flushPending();
        }, 0);
      }
      return next;
    });
  };

  const copyAll = () => {
    const text = lines.join('\n');
    const done = () => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    };
    if (navigator.clipboard?.writeText) {
      navigator.clipboard.writeText(text).then(done).catch(() => fallbackCopy(text, done));
    } else {
      fallbackCopy(text, done);
    }
  };

  const clearScreen = () => {
    pendingRef.current = [];
    setLines([]);
  };

  return (
    <div className="log-window">
      <div className="log-toolbar">
        <span className="log-title">实时日志（仅本窗口打开期间刷新）</span>
        <div className="log-toolbar-btns">
          <button className="log-btn" onClick={togglePause}>
            {paused ? '▶ 恢复' : '⏸ 暂停'}
          </button>
          <button className="log-btn" onClick={copyAll}>
            {copied ? '✓ 已复制' : '复制全部'}
          </button>
          <button className="log-btn" onClick={clearScreen}>
            清屏
          </button>
        </div>
      </div>
      <div className="log-list" ref={listRef} onScroll={onScroll}>
        {lines.map((l, i) => (
          <div className="log-line" key={i}>
            {l}
          </div>
        ))}
        {paused && (
          <div className="log-paused-badge">已暂停（期间日志已缓存，恢复后补上）</div>
        )}
      </div>
    </div>
  );
}

/** 旧 WebView 兜底：navigator.clipboard 不可用时用隐藏 textarea + execCommand */
function fallbackCopy(text: string, done: () => void) {
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.style.position = 'fixed';
  ta.style.opacity = '0';
  document.body.appendChild(ta);
  ta.select();
  try {
    document.execCommand('copy');
    done();
  } catch {
    /* 极端环境下复制失败：静默忽略 */
  }
  document.body.removeChild(ta);
}
