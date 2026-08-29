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
} from './api/tauri';

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
  const hideTimer = useRef<number | null>(null);

  const hideSelf = () => {
    void getCurrentWindow().hide();
  };

  // 显隐统一收归 Rust：挂载时若有遗留条目、或收到新事件，
  // 都通过 invoke('show_pull_toast') 让 Rust 负责 show + 定位（macOS 右上角 / Windows 右下角）。
  // 窗口仅在有待拉取内容时显示；清空后或用户手动关闭时收起。
  const showSelf = () => {
    void invoke('show_pull_toast');
  };

  useEffect(() => {
    listPendingOffers()
      .then((list) => {
        setOffers((prev) => {
          const have = new Set(prev.map((o) => o.transfer_id));
          return [...prev, ...list.filter((o) => !have.has(o.transfer_id))];
        });
        if (list.length > 0) showSelf();
      })
      .catch(() => {});

    // 跨 LAN 遗留条目：启动时若有，同样要弹出小窗
    listCrossLanOffers()
      .then((list) => {
        setCrossLan(list);
        if (list.length > 0) showSelf();
      })
      .catch(() => {})
      .finally(() => setReady(true));

    const un = [
      listen<PendingOffer>('file-offer', (e) => {
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

  // 全部清空且无进行中任务时，延迟自动关闭窗口
  useEffect(() => {
    if (!ready) return;
    if (
      offers.length > 0 ||
      pulling.size > 0 ||
      crossLan.length > 0 ||
      crossPulling.size > 0
    ) {
      if (hideTimer.current) {
        clearTimeout(hideTimer.current);
        hideTimer.current = null;
      }
      return;
    }
    // 停留 6 秒：给足时间看清"已保存到本地 / 拉取失败"的结果反馈。
    // 此前是 1.5 秒，自动拉取瞬间完成后小窗一闪而过，用户根本看不到 → 误判为"没弹"。
    hideTimer.current = window.setTimeout(hideSelf, 6000);
    return () => {
      if (hideTimer.current) clearTimeout(hideTimer.current);
    };
  }, [ready, offers, pulling, crossLan, crossPulling]);

  const onPull = (tid: string) => {
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
      <div className="pt-foot">拉取完成后自动收起，也可手动关闭</div>
    </div>
  );
}
