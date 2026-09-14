import { useEffect, useRef, useState } from 'react';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { getConfig, logWindowReady } from './api/tauri';
import { applyTheme } from './theme';

/**
 * 实时日志窗口（label = "log-viewer"，与 Rust `LOG_WINDOW_LABEL` 保持一致，改动需两处同步）。
 *
 * 数据源：Rust 侧文件 tail 任务（生命周期与窗口一致）——
 * - `log-line`：批量新行（string[]）
 * - `log-reset`：日志文件轮转/重建，清空当前视图
 *
 * 启动时序：先注册监听、再调 `log_window_ready` 拉历史并让 Rust 从该位置续推增量。
 * 顺序不能反——窗口刚创建时 WebView 尚未执行 JS，Rust 过早 emit 的事件会全部丢失
 * （曾导致打开窗口看不到任何历史）。
 *
 * 功能：自动滚底 + 暂停/恢复 + 复制全部 + 清屏。
 * 「不留存」：关闭窗口即销毁，无任何持久化；重开由 Rust 重放尾部历史。
 */

/** 视图中保留的最大行数（超出丢弃最旧的，防长时间运行内存膨胀） */
const MAX_VIEW_LINES = 5000;
/** 渲染节流：批量 append 的时间窗口（毫秒） */
const FLUSH_MS = 120;

/** 视图中的一行。id 单调自增——用下标当 key 会在截断列表时触发整表重渲染 */
interface Line {
  id: number;
  text: string;
}

export default function LogWindow() {
  const [lines, setLines] = useState<Line[]>([]);
  const [paused, setPaused] = useState(false);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  // 暂存待渲染的行：暂停期间只入队不渲染，恢复时一次性补上（不丢日志）
  const pendingRef = useRef<Line[]>([]);
  const flushTimerRef = useRef<number | null>(null);
  // 暂停状态镜像供事件回调读最新值（监听只挂载一次，闭包会捕获首帧值）
  const pausedRef = useRef(paused);
  pausedRef.current = paused;
  // 是否自动跟随底部：用户上滚查看历史时暂停跟随，滚回底部恢复
  const followRef = useRef(true);
  // 行 id 生成器
  const seqRef = useRef(0);

  /** 把一批新行入队，并按需触发节流渲染 */
  const pushLines = (texts: string[]) => {
    const pending = pendingRef.current;
    for (const t of texts) pending.push({ id: ++seqRef.current, text: t });
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
  };

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

  useEffect(() => {
    let disposed = false;
    const subs: UnlistenFn[] = [];

    // 与主窗口保持一致的主题（独立 WebView，需自行应用一次）
    getConfig()
      .then((c) => applyTheme(c.theme))
      .catch(() => {});

    void (async () => {
      try {
        // 1) 先注册监听（await 完成才算真正生效）
        const unLine = await listen<string[]>('log-line', (e) => {
          const batch = e.payload;
          if (!Array.isArray(batch) || batch.length === 0) return;
          pushLines(batch);
        });
        const unReset = await listen('log-reset', () => {
          pendingRef.current = [];
          setLines([]);
        });
        if (disposed) {
          unLine();
          unReset();
          return;
        }
        subs.push(unLine, unReset);

        // 2) 就绪信号：拉历史 + 让 Rust 从历史末尾续推增量（无缝、不重复）
        const history = await logWindowReady();
        if (disposed || history.length === 0) return;
        setLines(
          history.map((text) => ({ id: ++seqRef.current, text })).slice(-MAX_VIEW_LINES),
        );
      } catch (e) {
        // 监听/握手失败必须显性化：否则窗口只是一片空白，无从排查
        if (!disposed) setError(String(e));
      }
    })();

    return () => {
      disposed = true;
      subs.forEach((f) => f());
      if (flushTimerRef.current !== null) window.clearTimeout(flushTimerRef.current);
    };
    // pushLines/flushPending 只依赖 ref 与 setState，无需进依赖表
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

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
    const text = lines.map((l) => l.text).join('\n');
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
      {error && <div className="log-error">日志监听失败：{error}</div>}
      <div className="log-list" ref={listRef} onScroll={onScroll}>
        {lines.map((l) => (
          <div className="log-line" key={l.id}>
            {l.text}
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
