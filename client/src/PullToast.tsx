import { useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow, LogicalSize } from '@tauri-apps/api/window';
import { invoke } from '@tauri-apps/api/core';
import {
  listPendingOffers,
  pullFiles,
  PendingOffer,
  listCrossLanOffers,
  pullCrossLan,
  crossItemBase,
  crossItemId,
  CrossLanOffer,
  getConfig,
} from './api/tauri';

/** 未操作自动关闭的默认时长（毫秒），仅作为读取配置失败时的兜底 */
const DEFAULT_AUTO_HIDE_MS = 15_000;
/** 用户已点击拉取、操作完成（或失败）后，结果反馈停留时长（毫秒） */
const RESULT_HOLD_MS = 3_000;
/** 窗口内「同时展示」的条目总数上限（正在拉取的也占位） */
const MAX_TOTAL = 3;
/** 小窗宽度（逻辑像素），必须与 tauri.conf.json 中 pull-toast 的 width 一致 */
const WIN_W = 340;
/** 高度自适应区间：下限避免内容过少时窗口塌缩，上限避免撑满屏幕 */
const MIN_H = 76;
const MAX_H = 360;
/** 上下边框合计（.pull-toast 的 1px border × 2） */
const BORDER_H = 2;

function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function summary(o: PendingOffer): string {
  const names =
    o.top_names && o.top_names.length ? o.top_names : o.files.map((f) => f.file_name);
  if (names.length > 2) return `${names[0]} 等 ${names.length} 项`;
  return names.join('、');
}

function crossNames(o: CrossLanOffer): string {
  return (o.manifest || []).map((f) => f.file_name).join('、');
}

function crossTotal(o: CrossLanOffer): number {
  return (o.manifest || []).reduce(
    (s: number, f: { file_size: number }) => s + (f.file_size || 0),
    0,
  );
}

/** 小窗内的条目：本地 P2P 与跨 LAN 统一抽象，便于统一排序与数量裁剪 */
type Item =
  | { id: string; kind: 'local'; ts: number; offer: PendingOffer }
  | { id: string; kind: 'cross'; ts: number; offer: CrossLanOffer };

function itemNames(it: Item): string {
  return it.kind === 'local' ? summary(it.offer) : crossNames(it.offer) || '未知文件';
}

function itemSize(it: Item): number {
  return it.kind === 'local' ? it.offer.total_size : crossTotal(it.offer);
}

function itemFrom(it: Item): string {
  return it.kind === 'local' ? it.offer.device_name : `来自 ${it.offer.from_name || it.offer.from}（跨 LAN）`;
}

/**
 * 跨 LAN 条目的唯一 id 现由 `api/tauri.ts` 的 `crossItemId` 统一提供（带 `local:` 前缀），
 * 与后端进度事件 key 对齐；`crossItemBase` 用作拉取时传给后端的 pull_id（不带前缀）。
 * 不能只用 `from + ext_file_ep` —— 同一台设备（同一端口）连续发多个文件时该组合恒定，
 * 会把后续文件误判为重复而丢弃。
 */

export default function PullToast() {
  /** 待拉取（尚未开始拉取） */
  const [items, setItems] = useState<Item[]>([]);
  /** 正在拉取的条目 */
  const [pulling, setPulling] = useState<Item[]>([]);
  /** 拉取进行期间到达的新文件：先暂存，等拉取完成再补进待拉取列表 */
  const [queued, setQueued] = useState<Item[]>([]);
  /** 已完成/失败的结果反馈 */
  const [results, setResults] = useState<Record<string, { ok: boolean; msg: string }>>({});
  /** 已完成但保留「100% 进度条」可见片刻的条目（避免直接关闭看不到满） */
  const [completed, setCompleted] = useState<Record<string, Item>>({});
  const [progress, setProgress] = useState<Record<string, number>>({});
  const [ready, setReady] = useState(false);
  /** 用户是否已点击过「拉取」：点了就取消未操作倒计时，改为等任务完成 */
  const [userActed, setUserActed] = useState(false);
  /** 「未操作自动关闭」时长，来自配置项 toast_auto_hide_ms */
  const [autoHideMs, setAutoHideMs] = useState(DEFAULT_AUTO_HIDE_MS);
  /** 剩余自动关闭秒数（0 表示未启用倒计时） */
  const [countdown, setCountdown] = useState(0);
  /**
   * 每次「弹出/收到新文件」都自增。用于强制刷新倒计时：
   * 仅靠列表变化不够——同一设备重复通知可能不产生新的列表项，
   * 导致倒计时不重启（历史 bug：只有第一次会超时关闭）。
   */
  const [session, setSession] = useState(0);
  const pullingRef = useRef<Item[]>([]);
  pullingRef.current = pulling;

  // 高度自适应：测量「列表内容 + 页脚」的真实高度，反向设置窗口高度，
  // 消除固定 200px 造成的底部大片空白。
  const listRef = useRef<HTMLDivElement>(null);
  const footRef = useRef<HTMLDivElement>(null);
  /** 上一次已设置的高度，避免每帧/每秒倒计时都重复调用 setSize */
  const lastH = useRef(0);

  // [DEBUG] 诊断上报：把前端关键节点写进 Rust 日志，便于排查前端黑盒问题。
  const log = (m: string) => {
    if (import.meta.env.DEV) void invoke('debug_toast_log', { msg: m });
  };

  const hideSelf = () => {
    log('hideSelf 被调用（关闭按钮/自动收起）');
    // 走 Rust 命令而不是直接 window.hide()：macOS 上小窗曾是 key window，
    // 隐藏后系统会把主窗口顶上来，需要在 Rust 侧做补偿重新隐藏主窗口。
    void invoke('hide_pull_toast')
      .then(() => {
        log('hide_pull_toast 成功');
        // 关闭后重置「是否已点击拉取」，下次弹出重新计时
        setUserActed(false);
      })
      .catch((e: unknown) => log(`hide_pull_toast 失败: ${String(e)}`));
  };

  // 显隐统一收归 Rust：定位/置顶/提升都在 Rust 侧完成。
  // 每次调用都自增 session，确保倒计时一定重启。
  const showSelf = () => {
    void invoke('show_pull_toast');
    setSession((s) => s + 1);
  };

  /** 收到一个新条目：拉取进行中则暂存队列，否则直接显示 */
  const onNewItem = (it: Item) => {
    if (pullingRef.current.length > 0) {
      log(`拉取进行中，新文件先暂存：${it.id}`);
      setQueued((prev) =>
        prev.some((x) => x.id === it.id) ? prev : [...prev, it],
      );
    } else {
      log(`新条目入列：${it.id}`);
      setItems((prev) =>
        prev.some((x) => x.id === it.id)
          ? prev
          : [...prev, it].sort((a, b) => b.ts - a.ts),
      );
    }
    showSelf();
  };

  useEffect(() => {
    log(`useEffect 挂载（label=${getCurrentWindow().label}）`);

    getConfig()
      .then((c) => {
        const ms = c.toast_auto_hide_ms ?? DEFAULT_AUTO_HIDE_MS;
        setAutoHideMs(ms);
        log(`读到配置 toast_auto_hide_ms=${ms}`);
      })
      .catch((e: unknown) =>
        log(`读取配置失败，回落默认 ${DEFAULT_AUTO_HIDE_MS}ms: ${String(e)}`),
      );

    const now = () => Date.now();

    listPendingOffers()
      .then((list) => {
        log(`挂载 listPendingOffers -> ${list.length} 条`);
        const its: Item[] = list.map((o) => ({
          id: `local:${o.transfer_id}`,
          kind: 'local' as const,
          ts: now(),
          offer: o,
        }));
        if (its.length) {
          setItems((prev) => {
            const have = new Set(prev.map((x) => x.id));
            return [...prev, ...its.filter((x) => !have.has(x.id))].sort(
              (a, b) => b.ts - a.ts,
            );
          });
          showSelf();
        }
      })
      .catch((e: unknown) => log(`listPendingOffers 失败: ${String(e)}`));

    listCrossLanOffers()
      .then((list) => {
        log(`挂载 listCrossLanOffers -> ${list.length} 条`);
        const its: Item[] = list.map((o) => ({
          id: crossItemId(o),
          kind: 'cross' as const,
          ts: now(),
          offer: o,
        }));
        if (its.length) {
          setItems((prev) => {
            const have = new Set(prev.map((x) => x.id));
            return [...prev, ...its.filter((x) => !have.has(x.id))].sort(
              (a, b) => b.ts - a.ts,
            );
          });
          showSelf();
        }
      })
      .catch((e: unknown) => log(`listCrossLanOffers 失败: ${String(e)}`))
      .finally(() => setReady(true));

    const un = [
      listen<PendingOffer>('file-offer', (e) => {
        const o = e.payload;
        log(`收到 file-offer: ${o.transfer_id}`);
        onNewItem({
          id: `local:${o.transfer_id}`,
          kind: 'local',
          ts: now(),
          offer: o,
        });
      }),

      listen<CrossLanOffer>('cross-lan-file', (e) => {
        const o = e.payload;
        log(`收到 cross-lan-file: from=${o.from_name || o.from} files=${(o.manifest || []).length}`);
        onNewItem({ id: crossItemId(o), kind: 'cross', ts: now(), offer: o });
      }),

      listen<{ transfer_id: string }>('file-pull-start', (e) => {
        const id = `local:${e.payload.transfer_id}`;
        log(`file-pull-start: ${id}`);
        setItems((prev) => {
          const it = prev.find((x) => x.id === id);
          if (it) setPulling((p) => (p.some((x) => x.id === id) ? p : [...p, it]));
          return prev.filter((x) => x.id !== id);
        });
      }),

      listen<{ transfer_id: string; percent: number }>('file-pull-progress', (e) => {
        setProgress((prev) => ({
          ...prev,
          [`local:${e.payload.transfer_id}`]: e.payload.percent,
        }));
      }),

      listen<{ transfer_id: string; ok?: boolean; error?: string }>(
        'file-pull-complete',
        (e) => {
          const id = `local:${e.payload.transfer_id}`;
          const ok = e.payload.ok !== false;
          log(`file-pull-complete: ${id} ok=${ok}`);
          // 进度拉满到 100%，并把条目从 pulling 移到 completed：保留进度条可见 ~1.2s，
          // 让用户确实看到「100%」再转结果，而不是瞬间关闭。
          setProgress((prev) => ({ ...prev, [id]: 100 }));
          setPulling((prev) => {
            const it = prev.find((x) => x.id === id);
            if (it) setCompleted((c) => ({ ...c, [id]: it }));
            return prev.filter((x) => x.id !== id);
          });
          setItems((prev) => prev.filter((x) => x.id !== id));
          window.setTimeout(() => {
            setCompleted((c) => {
              const n = { ...c };
              delete n[id];
              return n;
            });
            setResults((prev) => ({
              ...prev,
              [id]: {
                ok,
                msg: ok ? '已保存到本地' : `拉取失败：${e.payload.error || '未知错误'}`,
              },
            }));
            setProgress((prev) => {
              const n = { ...prev };
              delete n[id];
              return n;
            });
          }, 1200);
        },
      ),

      listen<{ ext_file_ep: string; ok: boolean; error?: string }>(
        'cross-lan-pull-complete',
        (e) => {
          const ep = e.payload.ext_file_ep;
          log(`cross-lan-pull-complete: ep=${ep} ok=${e.payload.ok}`);
          setPulling((prev) => prev.filter((x) => !(x.kind === 'cross' && x.offer.ext_file_ep === ep)));
          setItems((prev) => prev.filter((x) => !(x.kind === 'cross' && x.offer.ext_file_ep === ep)));
          setResults((prev) => ({
            ...prev,
            [`cross-ep:${ep}`]: {
              ok: e.payload.ok,
              msg: e.payload.ok ? '已保存到本地' : `拉取失败：${e.payload.error || '未知错误'}`,
            },
          }));
        },
      ),
    ];

    return () => {
      un.forEach((p) => void p.then((f) => f()));
    };
  }, []);

  // 数量上限：窗口内总数不超过 MAX_TOTAL（正在拉取的也占位），
  // 即「有 1 个正在拉取时，待拉取只保留最新 2 个」。items 已按时间倒序，取前面即最新。
  useEffect(() => {
    const limit = Math.max(0, MAX_TOTAL - pulling.length);
    if (items.length > limit) {
      log(`条目超限，裁剪 ${items.length} -> ${limit}`);
      setItems((prev) => prev.slice(0, Math.max(0, MAX_TOTAL - pulling.length)));
    }
  }, [items, pulling]);

  // 拉取结束后，把暂存队列里的新文件补进待拉取列表（同样受上限约束）
  useEffect(() => {
    if (pulling.length > 0 || queued.length === 0) return;
    log(`拉取已结束，把 ${queued.length} 个暂存文件补入列表`);
    const incoming = queued;
    setQueued([]);
    setItems((prev) => {
      const have = new Set(prev.map((x) => x.id));
      return [...prev, ...incoming.filter((x) => !have.has(x.id))].sort(
        (a, b) => b.ts - a.ts,
      );
    });
  }, [pulling, queued]);

  // 关闭策略（三条规则）：
  //   1) 拉取进行中 → **绝不自动关闭**，等拉取完成并写入本机剪贴板
  //   2) 有待拉取条目、但用户未点「拉取」→ 未操作倒计时（配置项）后自动关闭
  //   3) 已点过拉取（完成/失败）或条目已清空 → 结果反馈停留片刻后关闭
  useEffect(() => {
    if (!ready) return;

    const busy = pulling.length > 0 || Object.keys(completed).length > 0;
    const hasItems = items.length > 0;

    // 1) 拉取中：保持显示，不倒计时
    if (busy) {
      log('拉取进行中 → 保持显示，等待任务完成');
      setCountdown(0);
      return;
    }

    // 2) 有待拉取但用户尚未点拉取 → 未操作倒计时
    if (hasItems && !userActed) {
      if (autoHideMs <= 0) {
        log('配置 toast_auto_hide_ms=0 → 不自动关闭，等用户操作');
        setCountdown(0);
        return;
      }
      const total = Math.max(1, Math.round(autoHideMs / 1000));
      log(`有待拉取条目，启动未操作倒计时 ${autoHideMs}ms（session=${session}）`);
      setCountdown(total);
      let left = total;
      const iv = window.setInterval(() => {
        left -= 1;
        setCountdown(left);
        if (left <= 0) {
          window.clearInterval(iv);
          log(`未操作超时（${autoHideMs}ms 内未点击拉取）→ 自动关闭`);
          hideSelf();
        }
      }, 1000);
      return () => window.clearInterval(iv);
    }

    // 3) 已点击拉取（完成/失败）或已无条目 → 结果反馈停留后关闭
    setCountdown(0);
    const t = window.setTimeout(() => {
      log(`结果反馈停留 ${RESULT_HOLD_MS}ms 结束 → 关闭`);
      hideSelf();
    }, RESULT_HOLD_MS);
    return () => window.clearTimeout(t);
  }, [ready, items, pulling, completed, results, userActed, autoHideMs, session]);

  // 窗口高度自适应内容：固定 200px 时，条目少会在列表与页脚之间留下大片空白。
  // 用 list.scrollHeight（内容超出时它仍是完整内容高度，而非被压缩后的可视高度）
  // 加页脚高度反推目标高度。
  // 关键：改完尺寸必须让 Rust 重新定位——它是按窗口「实际」尺寸贴右下角的，
  // 不重新定位的话底边/右边会错位（历史 bug 的根源就是这个尺寸不一致）。
  useEffect(() => {
    // 无内容时绝不弹窗/调尺寸：否则启动时空窗会被本 effect 弹出（footRef 有高度、
    // lastH=0 触发 setSize+show_pull_toast），显示「暂无待拉取文件」几秒后由关闭策略收起——
    // 表现为「一启动就闪一下空窗」。只在确有条目/进度/结果时才显示并自适应高度。
    const hasContent =
      items.length > 0 ||
      pulling.length > 0 ||
      Object.keys(results).length > 0 ||
      Object.keys(completed).length > 0;
    if (!hasContent) return;
    const listH = listRef.current?.scrollHeight ?? 0;
    const footH = footRef.current?.offsetHeight ?? 0;
    if (listH === 0 && footH === 0) return;
    const target = Math.min(MAX_H, Math.max(MIN_H, listH + footH + BORDER_H));
    if (lastH.current === target) return;
    lastH.current = target;
    log(`高度自适应: ${target}px（列表 ${listH} + 页脚 ${footH} + 边框 ${BORDER_H}）`);
    void getCurrentWindow()
      .setSize(new LogicalSize(WIN_W, target))
      .then(() => invoke('show_pull_toast'))
      .catch((e: unknown) => log(`高度自适应失败: ${String(e)}`));
  }, [items, pulling, results, completed, countdown, ready]);

  const onPull = (it: Item) => {
    // 已操作：取消未操作倒计时，改为「等拉取完成写入剪贴板后再关闭」
    setUserActed(true);
    if (it.kind === 'local') {
      const tid = it.offer.transfer_id;
      setPulling((prev) => (prev.some((x) => x.id === it.id) ? prev : [...prev, it]));
      setItems((prev) => prev.filter((x) => x.id !== it.id));
      setProgress((prev) => ({ ...prev, [it.id]: 0 }));
      setResults((prev) => {
        const n = { ...prev };
        delete n[it.id];
        return n;
      });
      pullFiles(tid).catch(() => {
        setPulling((prev) => prev.filter((x) => x.id !== it.id));
        setItems((prev) => (prev.some((x) => x.id === it.id) ? prev : [it, ...prev]));
        setProgress((prev) => {
          const n = { ...prev };
          delete n[it.id];
          return n;
        });
        setResults((prev) => ({
          ...prev,
          [it.id]: { ok: false, msg: '拉取失败，可重试' },
        }));
      });
    } else {
      const o = it.offer;
      setPulling((prev) => (prev.some((x) => x.id === it.id) ? prev : [...prev, it]));
      setItems((prev) => prev.filter((x) => x.id !== it.id));
      // 跨 LAN 拉取现在会实时上报进度；先把进度条初始化为 0，避免一上来就空白
      setProgress((prev) => ({ ...prev, [it.id]: 0 }));
      setResults((prev) => {
        const n = { ...prev };
        delete n[it.id];
        return n;
      });
      pullCrossLan(crossItemBase(o), o.ext_file_ep, o.manifest).catch(() => {
        setPulling((prev) => prev.filter((x) => x.id !== it.id));
        setItems((prev) => (prev.some((x) => x.id === it.id) ? prev : [it, ...prev]));
        setResults((prev) => ({
          ...prev,
          [it.id]: { ok: false, msg: '拉取失败，可重试' },
        }));
      });
    }
  };

  const empty =
    items.length === 0 &&
    pulling.length === 0 &&
    Object.keys(results).length === 0;

  return (
    <div className="pull-toast">
      <div className="pt-list" ref={listRef}>
        {empty && <div className="pt-empty">暂无待拉取文件</div>}

        {pulling.map((it) => {
          const pct = progress[it.id] ?? 0;
          return (
            <div className="pt-item" key={it.id}>
              <div className="pt-item-top">
                <span className="pt-name" title={itemNames(it)}>
                  {itemNames(it)}
                </span>
                <span className="pt-size">{fmtSize(itemSize(it))}</span>
              </div>
              <div className="pt-sub">{itemFrom(it)}</div>
              <div className="pt-bar">
                <div className="pt-bar-fill" style={{ width: `${pct}%` }} />
                <span className="pt-pct">{pct}%</span>
              </div>
            </div>
          );
        })}

        {Object.entries(completed).map(([id, it]) => (
          <div className="pt-item" key={id}>
            <div className="pt-item-top">
              <span className="pt-name" title={itemNames(it)}>
                {itemNames(it)}
              </span>
              <span className="pt-size">{fmtSize(itemSize(it))}</span>
            </div>
            <div className="pt-sub">{itemFrom(it)}</div>
            <div className="pt-bar">
              <div className="pt-bar-fill" style={{ width: '100%' }} />
              <span className="pt-pct">100%</span>
            </div>
          </div>
        ))}

        {items.map((it) => (
          <div className="pt-item" key={it.id}>
            <div className="pt-item-top">
              <span className="pt-name" title={itemNames(it)}>
                {itemNames(it)}
              </span>
              <span className="pt-size">{fmtSize(itemSize(it))}</span>
            </div>
            <div className="pt-sub">{itemFrom(it)}</div>
            {it.kind === 'local' && it.offer.auto_pull ? (
              <span className="pt-auto">自动拉取中…</span>
            ) : (
              <button className="pt-pull" onClick={() => onPull(it)}>
                拉取
              </button>
            )}
          </div>
        ))}

        {Object.entries(results).map(([k, r]) => (
          <div className="pt-item pt-result" key={k}>
            <span className={r.ok ? 'pt-ok' : 'pt-err'}>
              {r.ok ? '✓ ' : '✗ '}
              {r.msg}
            </span>
          </div>
        ))}
      </div>
      <div className="pt-foot" ref={footRef}>
        <span className="pt-foot-msg">
          {pulling.length > 0 ? (
            <span className="pt-foot-wait">等待任务完成后关闭…</span>
          ) : countdown > 0 ? (
            <span className="pt-foot-count">{countdown} 秒后自动关闭</span>
          ) : autoHideMs > 0 ? (
            <span>{Math.round(autoHideMs / 1000)} 秒内未点击将自动关闭</span>
          ) : (
            <span>不会自动关闭，需手动处理</span>
          )}
        </span>
        <button className="pt-close" onClick={hideSelf} title="关闭">
          ×
        </button>
      </div>
    </div>
  );
}
