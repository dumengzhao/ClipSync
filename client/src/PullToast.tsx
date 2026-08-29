import { useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { invoke } from '@tauri-apps/api/core';
import {
  listPendingOffers,
  pullFiles,
  PendingOffer,
  listCrossLanOffers,
  pullCrossLan,
  CrossLanOffer,
  getConfig,
} from './api/tauri';

/** 未操作自动关闭的默认时长（毫秒），仅作为读取配置失败时的兜底 */
const DEFAULT_AUTO_HIDE_MS = 15_000;
/** 用户已点击拉取、操作完成（或失败）后，结果反馈停留时长（毫秒） */
const RESULT_HOLD_MS = 3_000;

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

/** 跨 LAN 条目的稳定 key（与主窗口 App.tsx 保持一致） */
function crossKey(o: CrossLanOffer): string {
  return `${o.from}-${o.ext_file_ep}`;
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

export default function PullToast() {
  const [offers, setOffers] = useState<PendingOffer[]>([]);
  const [pulling, setPulling] = useState<Set<string>>(new Set());
  const [progress, setProgress] = useState<Record<string, number>>({});
  // 跨 LAN「待复制」条目。此前小窗完全不感知这套数据（只听本地 P2P 的 file-offer），
  // 而主窗口 App.tsx 会显示它 —— 这正是「主窗口能看到待拉取文件，小窗却不弹」的原因。
  const [crossLan, setCrossLan] = useState<CrossLanOffer[]>([]);
  const [crossPulling, setCrossPulling] = useState<Set<string>>(new Set());
  // 拉取完成/失败的结果反馈：完成后必须让用户明确看到结果，
  // 否则小窗一闪而过，用户会以为根本没弹出过。
  const [results, setResults] = useState<Record<string, { ok: boolean; msg: string }>>({});
  const [ready, setReady] = useState(false);
  // 用户是否已点击过「拉取」。点了就取消未操作倒计时，改为等拉取写完剪贴板再关闭。
  const [userActed, setUserActed] = useState(false);
  // 「未操作自动关闭」时长，来自配置项 toast_auto_hide_ms（动态可调），默认 15s
  const [autoHideMs, setAutoHideMs] = useState(DEFAULT_AUTO_HIDE_MS);
  const hideTimer = useRef<number | null>(null);

  // [DEBUG] 诊断上报：把前端关键节点写进 Rust 日志，便于排查
  // 「窗口弹了但内容为空 / 点关闭没反应」这类只看截图查不出的问题。
  const log = (m: string) => {
    if (import.meta.env.DEV) void invoke('debug_toast_log', { msg: m });
  };

  const hideSelf = () => {
    log('hideSelf 被调用（关闭按钮/自动收起）');
    void getCurrentWindow()
      .hide()
      .then(() => {
        log('getCurrentWindow().hide() 成功');
        // 关闭后重置「是否已点击拉取」，下次弹出时重新计时
        setUserActed(false);
      })
      .catch((e: unknown) => log(`hide() 失败: ${String(e)}`));
  };

  // 显隐统一收归 Rust：挂载时若有遗留条目、或收到新事件，
  // 都通过 invoke('show_pull_toast') 让 Rust 负责 show + 定位（macOS 右上角 / Windows 右下角）。
  // 窗口仅在有待拉取内容时显示；清空后或用户手动关闭时收起。
  const showSelf = () => {
    void invoke('show_pull_toast');
  };

  useEffect(() => {
    log(`useEffect 挂载（label=${getCurrentWindow().label}）`);

    // 「未操作自动关闭」时长走配置项，可在设置里动态调整
    getConfig()
      .then((c) => {
        const ms = c.toast_auto_hide_ms ?? DEFAULT_AUTO_HIDE_MS;
        setAutoHideMs(ms);
        log(`读到配置 toast_auto_hide_ms=${ms}`);
      })
      .catch((e: unknown) =>
        log(`读取配置失败，回落默认 ${DEFAULT_AUTO_HIDE_MS}ms: ${String(e)}`),
      );

    listPendingOffers()
      .then((list) => {
        log(`挂载 listPendingOffers -> ${list.length} 条`);
        setOffers((prev) => {
          const have = new Set(prev.map((o) => o.transfer_id));
          return [...prev, ...list.filter((o) => !have.has(o.transfer_id))];
        });
        if (list.length > 0) showSelf();
      })
      .catch((e: unknown) => log(`listPendingOffers 失败: ${String(e)}`));

    // 跨 LAN 遗留条目：启动时若有，同样要弹出小窗
    listCrossLanOffers()
      .then((list) => {
        log(`挂载 listCrossLanOffers -> ${list.length} 条`);
        setCrossLan(list);
        if (list.length > 0) showSelf();
      })
      .catch((e: unknown) => log(`listCrossLanOffers 失败: ${String(e)}`))
      .finally(() => setReady(true));

    const un = [
      listen<PendingOffer>('file-offer', (e) => {
        log(`收到 file-offer: ${e.payload.transfer_id}`);
        setOffers((prev) =>
          prev.some((o) => o.transfer_id === e.payload.transfer_id)
            ? prev
            : [...prev, e.payload],
        );
        showSelf();
      }),
      listen<{ transfer_id: string }>('file-pull-start', (e) => {
        setPulling((prev) => new Set(prev).add(e.payload.transfer_id));
      }),
      listen<{ transfer_id: string; percent: number }>('file-pull-progress', (e) => {
        setProgress((prev) => ({ ...prev, [e.payload.transfer_id]: e.payload.percent }));
      }),
      listen<{ transfer_id: string }>('file-pull-complete', (e) => {
        const tid = e.payload.transfer_id;
        setOffers((prev) => prev.filter((o) => o.transfer_id !== tid));
        setPulling((prev) => {
          const n = new Set(prev);
          n.delete(tid);
          return n;
        });
        // 完成后给出明确成功反馈并停留一段时间（此前 1.5s 就收起，用户来不及看到）
        setResults((prev) => ({ ...prev, [tid]: { ok: true, msg: '已保存到本地' } }));
        setTimeout(() => {
          setProgress((prev) => {
            const n = { ...prev };
            delete n[tid];
            return n;
          });
        }, 1200);
      }),
      // 跨 LAN 新文件到达（Rust 侧同时会调 show_pull_toast）
      listen<CrossLanOffer>('cross-lan-file', (e) => {
        const o = e.payload;
        log(
          `收到 cross-lan-file: from=${o.from_name || o.from} files=${
            (o.manifest || []).length
          }`,
        );
        setCrossLan((prev) =>
          prev.some((x) => crossKey(x) === crossKey(o)) ? prev : [...prev, o],
        );
        showSelf();
      }),
      // 跨 LAN 拉取完成 / 失败
      listen<{ ext_file_ep: string; ok: boolean; error?: string }>(
        'cross-lan-pull-complete',
        (e) => {
          const ep = e.payload.ext_file_ep;
          setCrossLan((prev) => prev.filter((x) => x.ext_file_ep !== ep));
          setCrossPulling((prev) => {
            const n = new Set(prev);
            n.delete(ep);
            return n;
          });
          setResults((prev) => ({
            ...prev,
            [`cross-${ep}`]: {
              ok: e.payload.ok,
              msg: e.payload.ok
                ? '已保存到本地'
                : `拉取失败：${e.payload.error || '未知错误'}`,
            },
          }));
        },
      ),
    ];
    return () => {
      un.forEach((p) => void p.then((f) => f()));
    };
  }, []);

  // 关闭策略（三条规则）：
  //   1) 拉取进行中 → **绝不关闭**，必须等拉取完成并写入本机剪贴板
  //   2) 有待拉取条目、但用户还没点「拉取」→ 未操作倒计时（配置项，默认 15s）后自动关闭
  //   3) 用户已点过拉取（完成或失败）、或条目已清空 → 结果反馈停留片刻后关闭
  useEffect(() => {
    if (!ready) return;

    const clear = () => {
      if (hideTimer.current) {
        clearTimeout(hideTimer.current);
        hideTimer.current = null;
      }
    };

    const busy = pulling.size > 0 || crossPulling.size > 0;
    const hasItems = offers.length > 0 || crossLan.length > 0;

    // 1) 拉取中：保持显示
    if (busy) {
      log('拉取进行中 → 保持显示，不自动关闭');
      clear();
      return;
    }

    // 2) 有待拉取但用户尚未点拉取 → 未操作倒计时
    if (hasItems && !userActed) {
      clear();
      if (autoHideMs <= 0) {
        log('配置 toast_auto_hide_ms=0 → 不自动关闭，等用户操作');
        return;
      }
      log(`有待拉取条目，启动未操作倒计时 ${autoHideMs}ms`);
      hideTimer.current = window.setTimeout(() => {
        log(`未操作超时（${autoHideMs}ms 内未点击拉取）→ 自动关闭`);
        hideSelf();
      }, autoHideMs);
      return;
    }

    // 3) 用户已点击拉取（完成/失败）或已无条目 → 结果反馈停留后关闭
    clear();
    hideTimer.current = window.setTimeout(() => {
      log(`结果反馈停留 ${RESULT_HOLD_MS}ms 结束 → 关闭`);
      hideSelf();
    }, RESULT_HOLD_MS);

    return clear;
  }, [ready, offers, pulling, crossLan, crossPulling, userActed, autoHideMs]);

  const onPull = (tid: string) => {
    // 已操作：取消未操作倒计时，改为「等拉取完成写入剪贴板后再关闭」
    setUserActed(true);
    setPulling((prev) => new Set(prev).add(tid));
    setProgress((prev) => ({ ...prev, [tid]: 0 }));
    setResults((prev) => {
      const n = { ...prev };
      delete n[tid];
      return n;
    });
    pullFiles(tid).catch(() => {
      // 拉取失败时退回待拉取态，让用户可重试
      setPulling((prev) => {
        const n = new Set(prev);
        n.delete(tid);
        return n;
      });
      setProgress((prev) => {
        const n = { ...prev };
        delete n[tid];
        return n;
      });
      // 失败必须可见：否则小窗静默消失，用户不知道文件没拉到
      setResults((prev) => ({ ...prev, [tid]: { ok: false, msg: '拉取失败，可重试' } }));
    });
  };

  const onCrossPull = (o: CrossLanOffer) => {
    const ep = o.ext_file_ep;
    // 已操作：取消未操作倒计时，改为「等拉取完成写入剪贴板后再关闭」
    setUserActed(true);
    setCrossPulling((prev) => new Set(prev).add(ep));
    setResults((prev) => {
      const n = { ...prev };
      delete n[`cross-${ep}`];
      return n;
    });
    pullCrossLan(ep, o.manifest).catch(() => {
      setCrossPulling((prev) => {
        const n = new Set(prev);
        n.delete(ep);
        return n;
      });
      setResults((prev) => ({
        ...prev,
        [`cross-${ep}`]: { ok: false, msg: '拉取失败，可重试' },
      }));
    });
  };

  const isEmpty =
    offers.length === 0 &&
    pulling.size === 0 &&
    crossLan.length === 0 &&
    crossPulling.size === 0 &&
    Object.keys(results).length === 0;

  return (
    <div className="pull-toast">
      <div className="pt-head">
        <span className="pt-title">待拉取文件</span>
        <button className="pt-close" onClick={hideSelf} title="关闭">
          ×
        </button>
      </div>
      <div className="pt-list">
        {isEmpty && <div className="pt-empty">暂无待拉取文件</div>}

        {offers.map((o) => {
          const isPulling = pulling.has(o.transfer_id);
          const pct = progress[o.transfer_id] ?? 0;
          return (
            <div className="pt-item" key={o.transfer_id}>
              <div className="pt-item-top">
                <span className="pt-name" title={summary(o)}>
                  {summary(o)}
                </span>
                <span className="pt-size">{fmtSize(o.total_size)}</span>
              </div>
              <div className="pt-sub">{o.device_name}</div>
              {isPulling ? (
                <div className="pt-bar">
                  <div className="pt-bar-fill" style={{ width: `${pct}%` }} />
                  <span className="pt-pct">{pct}%</span>
                </div>
              ) : o.auto_pull ? (
                <span className="pt-auto">自动拉取中…</span>
              ) : (
                <button className="pt-pull" onClick={() => onPull(o.transfer_id)}>
                  拉取
                </button>
              )}
            </div>
          );
        })}

        {crossLan.map((o) => {
          const isPulling = crossPulling.has(o.ext_file_ep);
          const names = crossNames(o);
          const total = crossTotal(o);
          return (
            <div className="pt-item" key={crossKey(o)}>
              <div className="pt-item-top">
                <span className="pt-name" title={names}>
                  {names || '未知文件'}
                </span>
                <span className="pt-size">{total > 0 ? fmtSize(total) : ''}</span>
              </div>
              <div className="pt-sub">来自 {o.from_name || o.from}（跨 LAN）</div>
              {isPulling ? (
                <span className="pt-auto">拉取中…</span>
              ) : (
                <button className="pt-pull" onClick={() => onCrossPull(o)}>
                  拉取
                </button>
              )}
            </div>
          );
        })}

        {Object.entries(results).map(([k, r]) => (
          <div className="pt-item pt-result" key={k}>
            <span className={r.ok ? 'pt-ok' : 'pt-err'}>
              {r.ok ? '✓ ' : '✗ '}
              {r.msg}
            </span>
          </div>
        ))}
      </div>
      <div className="pt-foot">
        {autoHideMs > 0
          ? `${Math.round(autoHideMs / 1000)} 秒内未点击拉取将自动关闭；点击后等拉取完成再关闭`
          : '不会自动关闭，需手动处理'}
      </div>
    </div>
  );
}
