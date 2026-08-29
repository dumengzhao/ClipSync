import { useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { invoke } from '@tauri-apps/api/core';
import { listPendingOffers, pullFiles, PendingOffer } from './api/tauri';

function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function summary(o: PendingOffer): string {
  const names =
    o.top_names && o.top_names.length
      ? o.top_names
      : o.files.map((f) => f.file_name);
  if (names.length > 2) return `${names[0]} 等 ${names.length} 项`;
  return names.join('、');
}

export default function PullToast() {
  const [offers, setOffers] = useState<PendingOffer[]>([]);
  const [pulling, setPulling] = useState<Set<string>>(new Set());
  const [progress, setProgress] = useState<Record<string, number>>({});
  const [ready, setReady] = useState(false);
  const hideTimer = useRef<number | null>(null);

  const hideSelf = () => {
    void getCurrentWindow().hide();
  };

  // 待拉取小窗的显示统一收归 Rust：挂载时若有遗留待拉取、或收到新的 file-offer，
  // 都通过 invoke('show_pull_toast') 让 Rust 负责 show + 定位（macOS 右上角 / Windows 右下角），
  // 不再由前端自行定位（此前「主窗口有、小窗不弹」的根因是定位/聚焦没在 Rust 侧完成）。
  // 窗口仅在有待拉取文件时才显示；待拉取清空（全部拉取完成）或用户手动关闭时收起。
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
      listen<{ transfer_id: string; percent: number }>(
        'file-pull-progress',
        (e) => {
          setProgress((prev) => ({ ...prev, [e.payload.transfer_id]: e.payload.percent }));
        },
      ),
      listen<{ transfer_id: string }>('file-pull-complete', (e) => {
        const tid = e.payload.transfer_id;
        setOffers((prev) => prev.filter((o) => o.transfer_id !== tid));
        setPulling((prev) => {
          const n = new Set(prev);
          n.delete(tid);
          return n;
        });
        // 完成后短暂保持 100% 显示，再移除进度条
        setTimeout(() => {
          setProgress((prev) => {
            const n = { ...prev };
            delete n[tid];
            return n;
          });
        }, 1200);
      }),
    ];
    return () => {
      un.forEach((p) => void p.then((f) => f()));
    };
  }, []);

  // 待拉取清空且无进行中任务时，延迟自动关闭窗口
  useEffect(() => {
    if (!ready) return;
    if (offers.length > 0 || pulling.size > 0) {
      if (hideTimer.current) {
        clearTimeout(hideTimer.current);
        hideTimer.current = null;
      }
      return;
    }
    hideTimer.current = window.setTimeout(hideSelf, 1500);
    return () => {
      if (hideTimer.current) clearTimeout(hideTimer.current);
    };
  }, [ready, offers, pulling]);

  const onPull = (tid: string) => {
    setPulling((prev) => new Set(prev).add(tid));
    setProgress((prev) => ({ ...prev, [tid]: 0 }));
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
    });
  };

  return (
    <div className="pull-toast">
      <div className="pt-head">
        <span className="pt-title">待拉取文件</span>
        <button className="pt-close" onClick={hideSelf} title="关闭">
          ×
        </button>
      </div>
      <div className="pt-list">
        {offers.length === 0 && pulling.size === 0 && (
          <div className="pt-empty">暂无待拉取文件</div>
        )}
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
      </div>
      <div className="pt-foot">拉取完成后自动收起，也可手动关闭</div>
    </div>
  );
}
