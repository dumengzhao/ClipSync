import { useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow, LogicalSize } from '@tauri-apps/api/window';
import { invoke } from '@tauri-apps/api/core';
import {
  listPendingOffers,
  pullFiles,
  cancelPull,
  cancelPullCrossLan,
  PendingOffer,
  listCrossLanOffers,
  pullCrossLan,
  crossItemBase,
  assertCrossPeerReachable,
  dropPendingOffer,
  crossItemId,
  CrossLanOffer,
  getConfig,
} from './api/tauri';

/** 未操作自动关闭的默认时长（毫秒），仅作为读取配置失败时的兜底 */
const DEFAULT_AUTO_HIDE_MS = 15_000;
/** 用户已点击拉取、操作完成（或失败）后，结果反馈停留时长（毫秒） */
const RESULT_HOLD_MS = 3_000;
/** 窗口内「同时展示」的条目总数上限（正在拉取的也占位）。
 *
 * 固定为 1：小窗只当「最新一条」的轻量提示——拉取中只显示正在拉取的那条（带进度），
 * 空闲时只显示最新到达的那条。完整清单（含来源徽章、清空按钮）在主窗口
 * 「待拉取文件」里，小窗不再堆叠多条（堆叠既占地方又要来回看）。
 */
const MAX_TOTAL = 1;
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
  const names = o.top_names && o.top_names.length ? o.top_names : o.files.map((f) => f.file_name);
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
  return it.kind === 'local'
    ? it.offer.device_name
    : `来自 ${it.offer.from_name || it.offer.from}（跨 LAN）`;
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
  /** 结果反馈阶段的剩余秒数（每秒刷新）。
   *
   * 此前该阶段把 countdown 置 0，页脚就停在「60 秒内未点击将自动关闭」不再变化 ——
   * 用户既看不到真实关闭时间、也不知道还要等多久（2026-09-23 反馈）。 */
  const [holdLeft, setHoldLeft] = useState(0);
  /** 已完成但保留「100% 进度条」可见片刻的条目（避免直接关闭看不到满） */
  const [completed, setCompleted] = useState<Record<string, Item>>({});
  const [progress, setProgress] = useState<Record<string, number>>({});
  /** 拉取路由标记（lan = 内网直连 / wan = 外网地址），键为条目 id */
  const [routes, setRoutes] = useState<Record<string, string>>({});
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
  /** 用户点「取消」：按条目**自身**推导要调的后端命令，前端状态由 file-pull-cancelled 事件统一收口。
   *
   * 不再依赖「点拉取时登记的取消回调」：拉取也可能从**主窗口**发起
   * （App.tsx 的 doPull / doCrossPull），此时小窗只是经 file-pull-start 事件被动把条目
   * 移进「拉取中」，没有任何地方登记回调——取消按钮就会静默无反应（2026-09-17 用户实测，
   * 那次拉取最后还是跑完了）。按 item 推导后，两条发起路径都能取消。
   */
  const onCancel = (it: Item) => {
    setUserActed(true);
    if (it.kind === 'local') {
      void cancelPull(it.offer.transfer_id);
    } else {
      void cancelPullCrossLan(crossItemBase(it.offer));
    }
  };

  /**
   * 对端报告的「部分文件传输失败」原因，键为条目 id。
   *
   * 用 ref 而非 state：`file-pull-error` 一定先于 `file-pull-complete` 到达，
   * 而 complete 处理里要**同步读到**这条错误才能把结果判成失败；若用 state，
   * 闭包里读到的是上一次渲染的旧值，会漏判。
   */
  const pullErrorsRef = useRef<Record<string, string>>({});

  // 高度自适应：测量「列表内容 + 页脚」的真实高度，反向设置窗口高度，
  // 消除固定 200px 造成的底部大片空白。
  const listRef = useRef<HTMLDivElement>(null);
  const footRef = useRef<HTMLDivElement>(null);
  /** 上一次已设置的高度，避免每帧/每秒倒计时都重复调用 setSize */
  const lastH = useRef(0);
  /** 我们是否「打算让窗口可见」：showSelf 置 true、hideSelf 置 false。
   *
   * 用于阻止「高度自适应」effect 把**已隐藏**的窗口重新显示出来 —— 那是真 bug：
   * 结果/条目变化引起内容变化 → effect 里 setSize 后无条件 show_pull_toast →
   * 刚收起的小窗又冒出来（2026-09-23 实测：报错收起的窗口自己弹回来两次）。 */
  const shownRef = useRef(false);

  // [DEBUG] 诊断上报：把前端关键节点写进 Rust 日志，便于排查前端黑盒问题。
  const log = (m: string) => {
    // release 也上报：否则「小窗里显示的错误」在日志里完全查不到
    // （2026-09-22 实测：用户看到拉取失败，而日志一行都没有）。
    void invoke('frontend_log', { msg: m });
  };

  const hideSelf = () => {
    log('hideSelf 被调用（关闭按钮/自动收起）');
    // 先标记「不打算可见」：这样后续内容变化触发的自适应不会再把它弹回来
    shownRef.current = false;
    // 清掉本次的「结果/完成/进度/路由」展示：小窗是**一次性通知**，收起后应回到干净状态。
    // 不清的话旧的失败结果会让「失败优先」分支一直命中 —— 下次弹出时**永远不进入倒计时**
    //（用户实测 2026-09-23：关窗后再复制，小窗不倒计时），旧完成框也会继续挂在那里。
    setResults({});
    setCompleted({});
    setRoutes({});
    setProgress({});
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

  // 事件监听/倒计时 useEffect 只在挂载时建立，直接引用会在闭包里捕获首帧的
  // onNewItem / hideSelf。用 ref 桥接：回调始终调用最新一次渲染的闭包，行为与
  // 「每次渲染重建监听」等价；且不能把它们加进 deps（每次渲染都是新引用，
  // 会导致监听反复重挂、倒计时不断重启），这是 exhaustive-deps 警告的根因。
  const hideSelfRef = useRef(hideSelf);
  hideSelfRef.current = hideSelf;

  // 显隐统一收归 Rust：定位/置顶/提升都在 Rust 侧完成。
  // 每次调用都自增 session，确保倒计时一定重启。
  const showSelf = () => {
    shownRef.current = true;
    void invoke('show_pull_toast');
    setSession((s) => s + 1);
  };

  /** 收到一个新条目：拉取进行中则暂存队列，否则直接显示 */
  const onNewItem = (it: Item) => {
    // 同 id 的旧结果先清掉：取消拉取后后端会把该条目退回（重发 offer），
    // 不清的话 `results` 里那条「已取消拉取」会残留，干扰「是否还有内容」的判断。
    setResults((prev) => {
      if (!(it.id in prev)) return prev;
      const n = { ...prev };
      delete n[it.id];
      return n;
    });
    // **completed 也要一起清**：只清 results 会留下一条陈旧完成框 ——
    // 它没有结果可渲染，就会显示「路由 + 100% 进度条」一直挂着，而内容明明失败了
    //（用户实测 2026-09-23）。条目现在重新回到 items（待拉取）✓
    setCompleted((prev) => {
      if (!(it.id in prev)) return prev;
      const n = { ...prev };
      delete n[it.id];
      return n;
    });
    if (pullingRef.current.length > 0) {
      log(`拉取进行中，新文件先暂存：${it.id}`);
      setQueued((prev) => (prev.some((x) => x.id === it.id) ? prev : [...prev, it]));
    } else {
      log(`新条目入列：${it.id}`);
      setItems((prev) =>
        prev.some((x) => x.id === it.id) ? prev : [...prev, it].sort((a, b) => b.ts - a.ts),
      );
    }
    showSelf();
  };

  const onNewItemRef = useRef(onNewItem);
  onNewItemRef.current = onNewItem;

  useEffect(() => {
    log(`useEffect 挂载（label=${getCurrentWindow().label}）`);

    getConfig()
      .then((c) => {
        const ms = c.toast_auto_hide_ms ?? DEFAULT_AUTO_HIDE_MS;
        setAutoHideMs(ms);
        log(`读到配置 toast_auto_hide_ms=${ms}`);
      })
      .catch((e: unknown) => log(`读取配置失败，回落默认 ${DEFAULT_AUTO_HIDE_MS}ms: ${String(e)}`));

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
            return [...prev, ...its.filter((x) => !have.has(x.id))].sort((a, b) => b.ts - a.ts);
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
            return [...prev, ...its.filter((x) => !have.has(x.id))].sort((a, b) => b.ts - a.ts);
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
        onNewItemRef.current({
          id: `local:${o.transfer_id}`,
          kind: 'local',
          ts: now(),
          offer: o,
        });
      }),

      listen<CrossLanOffer>('cross-lan-file', (e) => {
        const o = e.payload;
        log(
          `收到 cross-lan-file: from=${o.from_name || o.from} files=${(o.manifest || []).length}`,
        );
        onNewItemRef.current({ id: crossItemId(o), kind: 'cross', ts: now(), offer: o });
      }),

      // 后端丢弃某条跨 LAN 通知（失败即删 / 上限淘汰）：从本窗口移除，
      // 这样另一侧触发的删除也能同步过来（只清待拉取条目，失败结果的展示保留）。
      listen<{
        from: string;
        ext_file_ep: string;
        manifest: CrossLanOffer['manifest'];
        from_name?: string;
        reason?: string;
      }>('cross-lan-offer-dropped', (e) => {
        const p = e.payload;
        const id = crossItemId({
          from: p.from,
          ext_file_ep: p.ext_file_ep,
          manifest: p.manifest,
        } as CrossLanOffer);
        log(`cross-lan-offer-dropped: reason=${p.reason || '?'}`);
        setItems((prev) => prev.filter((x) => x.id !== id));
      }),

      listen<{ transfer_id: string; route?: string }>('file-pull-start', (e) => {
        const id = `local:${e.payload.transfer_id}`;
        if (e.payload.route) {
          // 路由在 start 事件即确定：徽标从进度条一开始就可见，不用等首个进度帧
          setRoutes((prev) => ({ ...prev, [id]: e.payload.route as string }));
        }
        log(`file-pull-start: ${id}`);
        setItems((prev) => {
          const found = prev.find((x) => x.id === id);
          if (found) setPulling((p) => (p.some((x) => x.id === id) ? p : [...p, found]));
          if (found) return prev.filter((x) => x.id !== id);
          // 条目已不在待拉取列表（例如条目超过展示上限被裁、或快照未加载完）：
          // 用事件载荷造一个最小条目，确保**进度与「取消」按钮一定出现**。
          // 取消只依赖 transfer_id，不需要完整文件清单。
          const fallback: Item = {
            id,
            kind: 'local',
            ts: Date.now(),
            offer: {
              transfer_id: e.payload.transfer_id,
              device_id: '',
              device_name: '',
              files: [],
              total_size: 0,
              // 名字兜底：无清单时进度行不至于只剩「0 B | 内网 | 37%」没有主体
              top_names: ['（从其它窗口发起）'],
            },
          };
          setPulling((p) => (p.some((x) => x.id === id) ? p : [...p, fallback]));
          return prev;
        });
      }),

      listen<{ transfer_id: string; percent: number; route?: string }>(
        'file-pull-progress',
        (e) => {
          const id = `local:${e.payload.transfer_id}`;
          setProgress((prev) => ({
            ...prev,
            [id]: e.payload.percent,
          }));
          // 路由标记（lan/wan）：让用户能看出这次拉取走的是内网还是外网地址
          if (e.payload.route) {
            setRoutes((prev) => ({ ...prev, [id]: e.payload.route as string }));
          }
        },
      ),

      listen<{
        transfer_id: string;
        message: string;
        failed_files: string[];
        /** true = 传输彻底没跑起来（条目失效/对端未连接），**不会有** complete 跟随 */
        fatal?: boolean;
      }>('file-pull-error', (e) => {
        const id = `local:${e.payload.transfer_id}`;
        const msg = e.payload.message;
        log(`file-pull-error: ${id} fatal=${e.payload.fatal === true} ${msg}`);
        if (e.payload.fatal === true) {
          // 不会有 complete 到达：必须立刻把条目从「拉取中」移出并给出结果，
          // 否则小窗会一直停在「等待任务完成后关闭…」，看起来像卡死。
          setPulling((prev) => prev.filter((x) => x.id !== id));
          setProgress((prev) => {
            const n = { ...prev };
            delete n[id];
            return n;
          });
          setResults((prev) => ({ ...prev, [id]: { ok: false, msg } }));
          return;
        }
        // 部分失败：仅记录原因，等随后的 file-pull-complete 收口展示结果
        pullErrorsRef.current[id] = msg;
      }),

      listen<{ transfer_id: string; ok?: boolean; error?: string; route?: string }>(
        'file-pull-complete',
        (e) => {
          const id = `local:${e.payload.transfer_id}`;
          // 取走（并清除）本次传输的错误原因：有则判失败、展示对端给出的具体原因
          const err = pullErrorsRef.current[id];
          if (err) delete pullErrorsRef.current[id];
          const ok = e.payload.ok !== false && !err;
          log(`file-pull-complete: ${id} ok=${ok}`);
          // 进度拉满到 100%，并把条目从 pulling 移到 completed：保留进度条可见 ~1.2s，
          // 让用户确实看到「100%」再转结果，而不是瞬间关闭。
          setProgress((prev) => ({ ...prev, [id]: 100 }));
          if (e.payload.route) {
            setRoutes((prev) => ({ ...prev, [id]: e.payload.route as string }));
          }
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
                msg: ok
                  ? `已保存到本地${
                      e.payload.route === 'lan'
                        ? '（内网直连）'
                        : e.payload.route === 'wan'
                          ? '（外网地址）'
                          : ''
                    }`
                  : err
                    ? `部分文件传输失败：${err}`
                    : `拉取失败：${e.payload.error || '未知错误'}`,
              },
            }));
            if (!ok) {
              log(`file-pull-complete 显示为失败：${err || e.payload.error || '未知错误'}`);
            }
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
          setPulling((prev) =>
            prev.filter((x) => !(x.kind === 'cross' && x.offer.ext_file_ep === ep)),
          );
          setItems((prev) =>
            prev.filter((x) => !(x.kind === 'cross' && x.offer.ext_file_ep === ep)),
          );
          setResults((prev) => ({
            ...prev,
            [`cross-ep:${ep}`]: {
              ok: e.payload.ok,
              msg: e.payload.ok ? '已保存到本地' : `拉取失败：${e.payload.error || '未知错误'}`,
            },
          }));
        },
      ),

      // 用户主动取消拉取（P2P 与跨 LAN 共用此事件）。此处统一收口：
      // 移出「拉取中」、清进度、展示「已取消」结果，行为与失败类似但文案区分。
      listen<{ transfer_id: string }>('file-pull-cancelled', (e) => {
        const id = `local:${e.payload.transfer_id}`;
        log(`file-pull-cancelled: ${id}`);
        // 从「拉取中」摘除条目（若还在）：保留一份引用给结果展示
        setPulling((prev) => {
          const it = prev.find((x) => x.id === id);
          if (it) {
            window.setTimeout(() => {
              setResults((prev) => ({
                ...prev,
                [id]: { ok: false, msg: '已取消拉取' },
              }));
              setProgress((prev) => {
                const n = { ...prev };
                delete n[id];
                return n;
              });
            }, 0);
          }
          return prev.filter((x) => x.id !== id);
        });
        setItems((prev) => prev.filter((x) => x.id !== id));
        setProgress((prev) => {
          const n = { ...prev };
          delete n[id];
          return n;
        });
      }),
      // 主窗口「清空」后同步小窗：两段清单（本地 P2P + 跨 LAN）是一次清空的，
      // 这里整表清掉；正在拉取的条目在 pulling 里，不受影响。
      listen<number>('pending-offers-cleared', () => {
        setItems([]);
        // 暂存队列同样要清：它是「拉取期间到达、待补显示」的条目，后端清单已被清空，
        // 不清的话拉取一结束它们又会冒出来（用户刚点过清空）。
        setQueued([]);
      }),
    ];

    return () => {
      un.forEach((p) => void p.then((f) => f()));
    };
  }, []);

  // 数量上限：窗口内**显示**不超过 MAX_TOTAL（正在拉取的也占位）。
  // MAX_TOTAL = 1 → 「有正在拉取的，就只显示正在拉取的；否则只显示最新的一条」。
  //
  // 注意：这里只裁剪**显示**，不从 state 里删条目。`file-pull-start` 是「从 items 里
  // 找到该条目移进 pulling」，若把条目真删了，从主窗口发起的拉取就会因为找不到条目而
  // 完全不显示进度（2026-09-17 一并修掉）。
  const visiblePending = items.slice(0, Math.max(0, MAX_TOTAL - pulling.length));

  // 拉取结束后，把暂存队列里的新文件补进列表（受上限约束 → 只留最新的一条）。
  // 拉取过程中到达的文件**不显示**（只显示正在拉取的那条），避免打断进度视图。
  useEffect(() => {
    if (pulling.length > 0 || queued.length === 0) return;
    log(`拉取已结束，把 ${queued.length} 个暂存文件补入列表（上限 ${MAX_TOTAL} 条）`);
    const incoming = queued;
    setQueued([]);
    setItems((prev) => {
      const have = new Set(prev.map((x) => x.id));
      return [...prev, ...incoming.filter((x) => !have.has(x.id))].sort((a, b) => b.ts - a.ts);
    });
  }, [pulling, queued]);

  // 关闭策略（三条规则）：
  //   1) 拉取进行中 → **绝不自动关闭**，等拉取完成并写入本机剪贴板
  //   2) 有待拉取条目、但用户未点「拉取」→ 未操作倒计时（配置项）后自动关闭
  //   3) 已点过拉取（完成/失败）或条目已清空 → 结果反馈停留片刻后关闭
  useEffect(() => {
    if (!ready) return;

    // 0) **失败 → 取消一切自动关闭，只能手动关**（用户要求 2026-09-23：
    //    「弹出错误就销毁定时任务只能手动关闭」）。
    //    必须放在 busy 判断**之前**：失败条目现在也进 completed，会命中 busy 分支而被
    //    "保持显示"，但我们要的是明确语义 —— 不倒计时、不自动关，用户看清后自己点 ×。
    if (Object.values(results).some((r) => r && r.ok === false)) {
      log('结果区含失败 → 取消自动关闭（只能手动关闭）');
      setCountdown(0);
      setHoldLeft(0);
      return;
    }

    const busy = pulling.length > 0 || Object.keys(completed).length > 0;
    const hasItems = items.length > 0;

    // 1) 拉取中：保持显示，不倒计时
    if (busy) {
      log('拉取进行中 → 保持显示，等待任务完成');
      setCountdown(0);
      setHoldLeft(0);
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
      // 清掉可能残留的「结果反馈」秒数：否则页脚优先显示 holdLeft，
      // 会把 60 秒的未操作倒计时显示成一个陈旧的短秒数（用户实测 2026-09-23）。
      setHoldLeft(0);
      let left = total;
      const iv = window.setInterval(() => {
        left -= 1;
        setCountdown(left);
        if (left <= 0) {
          window.clearInterval(iv);
          log(`未操作超时（${autoHideMs}ms 内未点击拉取）→ 自动关闭`);
          hideSelfRef.current();
        }
      }, 1000);
      return () => window.clearInterval(iv);
    }

    // 3) 有结果反馈（完成/失败）→ 停留后关闭。
    //
    // **必须加前置条件**：此前这里是无条件兜底 —— 窗口已收起、结果也已被 hideSelf 清空时，
    // 它仍会 arm 一个 3 秒定时器并把 holdLeft 置成 3，于是下次弹出时页脚先显示这个陈旧秒数
    // （表现为「倒计时只剩几秒」），那个定时器到点还会把新弹出的窗口提前关掉
    //（用户实测 2026-09-23）。
    const hasResult = Object.keys(results).length > 0 || Object.keys(completed).length > 0;
    if (!shownRef.current || !hasResult) {
      setCountdown(0);
      setHoldLeft(0);
      return;
    }
    setCountdown(0);
    // 失败已在上方提前返回（改为手动关闭），走到这里的只有「无条目」或成功结果 → 统一 3 秒。
    const hold = RESULT_HOLD_MS;
    // 让「还有多久关闭」可见：每秒递减（否则页脚一直显示那句 60 秒的旧文案）
    setHoldLeft(Math.ceil(hold / 1000));
    const iv = window.setInterval(() => {
      setHoldLeft((s) => (s > 0 ? s - 1 : 0));
    }, 1000);
    const t = window.setTimeout(() => {
      log(`结果反馈停留 ${hold}ms 结束 → 关闭`);
      hideSelfRef.current();
    }, hold);
    return () => {
      window.clearTimeout(t);
      window.clearInterval(iv);
      setHoldLeft(0);
    };
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
      .then(() => {
        // 只让**本来可见**的窗口保持尺寸正确；绝不复活已隐藏的窗口
        // （否则「失败后条目/结果变化」会把刚收起的小窗又弹出来）。
        if (shownRef.current) return invoke('show_pull_toast');
        return undefined;
      })
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
      pullFiles(tid).catch((e: unknown) => {
        // 失败即终结（用户要求 2026-09-23）：**不放回**待拉取列表（否则混在一起看不出哪条失败），
        // 就在原框里显示「哪条 + 什么原因」，并让后端把该条目删掉 —— 想再要就重新复制。
        log(`本地拉取失败：${itemNames(it)} —— ${String(e)}`);
        setPulling((prev) => prev.filter((x) => x.id !== it.id));
        setProgress((prev) => {
          const n = { ...prev };
          delete n[it.id];
          return n;
        });
        // 把条目放进 completed：那里渲染**完整条目框**（文件名/来源/大小），只把「拉取」
        // 按钮的位置换成错误文案 —— 用户要求保留原待拉取信息，别把整条 UI 删掉（2026-09-23）。
        setCompleted((c) => ({ ...c, [it.id]: it }));
        setResults((prev) => ({
          ...prev,
          [it.id]: { ok: false, msg: `拉取失败：${String(e)}` },
        }));
        void dropPendingOffer(tid).catch((err: unknown) =>
          log(`dropPendingOffer 失败：${String(err)}`),
        );
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
      // 先探对端可达性，再拉取：失败时把「哪条地址、什么原因」显示出来，
      // 而不是只给一句「拉取失败，可重试」（用户实测 2026-09-22）。
      assertCrossPeerReachable(o)
        .then(() => pullCrossLan(crossItemBase(o), o.from, o.ext_file_ep, o.manifest))
        .catch((e: unknown) => {
          // 失败即终结：不放回列表；后端在失败分支已把该通知从待复制清单删除
          // （见 tauri_cmd::pull_cross_lan）。
          log(`跨 LAN 拉取失败：${itemNames(it)} —— ${String(e)}`);
          setPulling((prev) => prev.filter((x) => x.id !== it.id));
          // 同上：保留完整条目框，只在按钮位置显示错误
          setCompleted((c) => ({ ...c, [it.id]: it }));
          setResults((prev) => ({
            ...prev,
            [it.id]: { ok: false, msg: `拉取失败：${String(e)}` },
          }));
        });
    }
  };

  const empty = items.length === 0 && pulling.length === 0 && Object.keys(results).length === 0;

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
              <div
                style={{
                  display: 'flex',
                  alignItems: 'center',
                  gap: '0.4rem',
                  marginTop: '0.5rem',
                }}
              >
                {routes[it.id] && (
                  <span
                    style={{
                      fontSize: '0.72rem',
                      flexShrink: 0,
                      whiteSpace: 'nowrap',
                      height: '16px',
                      lineHeight: '16px',
                      fontWeight: 600,
                      color: routes[it.id] === 'lan' ? '#2563eb' : '#fbbf24',
                    }}
                  >
                    {routes[it.id] === 'lan' ? '内网' : '外网'}
                  </span>
                )}
                <div className="pt-bar" style={{ flex: 1, marginTop: 0 }}>
                  <div className="pt-bar-fill" style={{ width: `${pct}%` }} />
                  <span className="pt-pct">{pct}%</span>
                </div>
                <button className="pt-cancel" onClick={() => onCancel(it)} title="取消拉取">
                  取消
                </button>
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
            {/* 失败：保留上面的文件名/来源/大小，只把「拉取」按钮的位置换成错误文案 */}
            {results[id]?.ok === false ? (
              <div style={{ marginTop: '0.5rem' }}>
                <span className="pt-err">✗ {results[id].msg}</span>
              </div>
            ) : results[id]?.ok === true ? (
            <div
              style={{ display: 'flex', alignItems: 'center', gap: '0.4rem', marginTop: '0.5rem' }}
            >
              {routes[it.id] && (
                <span
                  style={{
                    fontSize: '0.72rem',
                    flexShrink: 0,
                    whiteSpace: 'nowrap',
                    height: '16px',
                    lineHeight: '16px',
                    fontWeight: 600,
                    color: routes[it.id] === 'lan' ? '#2563eb' : '#fbbf24',
                  }}
                >
                  {routes[it.id] === 'lan' ? '内网' : '外网'}
                </span>
              )}
              <div className="pt-bar" style={{ flex: 1, marginTop: 0 }}>
                <div className="pt-bar-fill" style={{ width: '100%' }} />
                <span className="pt-pct">100%</span>
              </div>
            </div>
            ) : null}
            {/* 成功文案也在这个框里显示：上一轮把裸文本结果区改成"只兜底"后，
                「✓ 已保存到本地（内网直连）」被过滤掉了（回归，2026-09-23 一并修）。 */}
            {results[id]?.ok === true ? (
              <div style={{ marginTop: '0.35rem' }}>
                <span className="pt-ok">✓ {results[id].msg}</span>
              </div>
            ) : null}
          </div>
        ))}

        {visiblePending.map((it) => (
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

        {/* 兜底：找不到条目可渲染时的结果文本（例如 fatal 且条目已不存在）。
            正常失败条目已在 completed 里带完整信息渲染过，这里过滤掉避免重复显示。 */}
        {Object.entries(results)
          .filter(([k]) => !(k in completed))
          .map(([k, r]) => (
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
          ) : Object.values(results).some((r) => r && r.ok === false) ? (
            <span className="pt-foot-wait">拉取失败 — 请手动关闭</span>
          ) : holdLeft > 0 ? (
            <span className="pt-foot-count">{holdLeft} 秒后自动关闭</span>
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
