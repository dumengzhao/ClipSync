import { useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
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
  const idleTimer = useRef<number | null>(null);

  const hideSelf = () => {
    if (idleTimer.current) {
      clearTimeout(idleTimer.current);
      idleTimer.current = null;
    }
    void getCurrentWindow().hide();
  };

  useEffect(() => {
    listPendingOffers()
      .then((list) => {
        setOffers((prev) => {
          const have = new Set(prev.map((o) => o.transfer_id));
          return [...prev, ...list.filter((o) => !have.has(o.transfer_id))];
        });
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

  // 纯待拉取态且长时间无操作：自动收起小窗（toast 行为，避免一直常驻遮挡右下角）。
  // 拉取中（pulling 非空）或列表已清空时不启用该计时。
  useEffect(() => {
    if (!ready) return;
    if (offers.length === 0 || pulling.size > 0) {
      if (idleTimer.current) {
        clearTimeout(idleTimer.current);
        idleTimer.current = null;
      }
      return;
    }
    idleTimer.current = window.setTimeout(hideSelf, 10000);
    return () => {
      if (idleTimer.current) clearTimeout(idleTimer.current);
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
              ) : (
                <button className="pt-pull" onClick={() => onPull(o.transfer_id)}>
                  拉取
                </button>
              )}
            </div>
          );
        })}
      </div>
      <div className="pt-foot">10 秒无操作将自动收起，待拉取文件可在主界面处理</div>
    </div>
  );
}
