import { create } from "zustand";
import type { components } from "@/api/schema";
import { wsClient } from "@/lib/ws";

type MetricSnapshot = components["schemas"]["MetricSnapshot"];

/** 序列键：`metric|labels`（labels 为空串时就是 `metric|`）。 */
export function seriesKey(metric: string, labels: string): string {
  return `${metric}|${labels}`;
}

export interface Ring {
  /** unix 秒，升序 */
  readonly ts: readonly number[];
  readonly v: readonly number[];
}

/** 环形缓冲长度：2s 一帧 × 90 ≈ 3 分钟，足够 60 秒视图与 sparkline。 */
const CAP = 90;

interface LiveState {
  /** 序列键 → 环形缓冲 */
  rings: ReadonlyMap<string, Ring>;
  /** 最新一帧的时刻（unix 秒）；0 = 还没收到任何帧 */
  lastTs: number;
  /** WS 是否在线 */
  up: boolean;
  ingest: (snap: MetricSnapshot) => void;
  setUp: (up: boolean) => void;
  clear: () => void;
}

export const useLive = create<LiveState>((set, get) => ({
  rings: new Map(),
  lastTs: 0,
  up: false,
  ingest: (snap) => {
    const prev = get().rings;
    const next = new Map(prev);
    for (const val of snap.values ?? []) {
      if (typeof val.value !== "number") continue;
      const key = seriesKey(val.metric, val.labels ?? "");
      const old = next.get(key);
      const ts = old ? [...old.ts, snap.ts] : [snap.ts];
      const v = old ? [...old.v, val.value] : [val.value];
      if (ts.length > CAP) {
        ts.splice(0, ts.length - CAP);
        v.splice(0, v.length - CAP);
      }
      next.set(key, { ts, v });
    }
    set({ rings: next, lastTs: snap.ts });
  },
  setUp: (up) => set({ up }),
  clear: () => set({ rings: new Map(), lastTs: 0 }),
}));

let detach: (() => void) | null = null;

/**
 * 登录后调用：订阅 metrics.live 灌进 store；WS 不健康（未连上或 6 秒没有帧）时
 * 退化为 REST 轮询 `/metrics/current`——页面照常工作，「实时」方块保持灰色示警。
 */
export function startLive(): void {
  stopLive();
  const offData = wsClient.subscribe("metrics.live", {}, (payload) => {
    useLive.getState().ingest(payload as MetricSnapshot);
  });
  const offStatus = wsClient.onStatus((up) => useLive.getState().setUp(up));
  const poller = setInterval(() => {
    const st = useLive.getState();
    const stale = st.lastTs === 0 || Date.now() / 1000 - st.lastTs > 6;
    if (!stale) return;
    void (async () => {
      const { api } = await import("@/api/client");
      const { data } = await api.GET("/api/v1/metrics/current");
      // 只在仍然陈旧时并入，避免和迟到的 WS 帧打架
      const cur = useLive.getState();
      if (data && (cur.lastTs === 0 || data.ts > cur.lastTs)) cur.ingest(data);
    })();
  }, 2_000);
  detach = () => {
    offData();
    offStatus();
    clearInterval(poller);
  };
}

export function stopLive(): void {
  detach?.();
  detach = null;
  useLive.getState().clear();
}

/** 取一个序列的最新值。 */
export function latestOf(rings: ReadonlyMap<string, Ring>, key: string): number | null {
  const r = rings.get(key);
  const v = r?.v[r.v.length - 1];
  return typeof v === "number" ? v : null;
}

/** 按指标名汇总最新值（多标签求和）。找不到返回 null。 */
export function latestSum(rings: ReadonlyMap<string, Ring>, metric: string): number | null {
  let sum = 0;
  let seen = false;
  for (const [key, r] of rings) {
    if (key.startsWith(`${metric}|`)) {
      const v = r.v[r.v.length - 1];
      if (typeof v === "number") {
        sum += v;
        seen = true;
      }
    }
  }
  return seen ? sum : null;
}
