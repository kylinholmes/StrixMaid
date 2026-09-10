import { useEffect, useRef, useState } from "react";
import { api } from "@/api/client";
import { wsClient } from "@/lib/ws";
import type { Proc } from "./tree";

/** 与 `GET /processes` / WS `processes.live` 相同的查询面(排序在客户端做,不进查询)。 */
export interface ProcQuery {
  user?: string;
  q?: string;
  tree?: boolean;
}

/** 每帧上限:WS 协议的 PROC_LIVE_MAX_LIMIT。列表页一屏也看不了这么多。 */
const LIMIT = 500;
/** 超过这个秒数没有 WS 帧就算陈旧,REST 轮询顶上(套路同 metrics 的 live 层)。 */
const STALE_SECS = 7;

export interface ProcLive {
  rows: readonly Proc[];
  /** 是否收到过至少一帧(骨架屏判断)。改筛选**不**复位——旧行留着,避免整表闪骨架屏。 */
  loaded: boolean;
  /** WS 在线(离线时数据来自 REST 兜底,更新更慢) */
  up: boolean;
}

/**
 * 订阅 `processes.live`,查询参数变了就重订阅;WS 不健康时退 REST 轮询。
 * 帧就是平铺的 `ProcessSummary[]`,直接整表替换——不做增量合并。
 */
export function useProcLive(query: ProcQuery): ProcLive {
  const [rows, setRows] = useState<readonly Proc[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [up, setUp] = useState(wsClient.up);
  const lastAt = useRef(0);
  const key = JSON.stringify(query);

  // biome-ignore lint/correctness/useExhaustiveDependencies: key 串已覆盖 query 全部字段
  useEffect(() => {
    lastAt.current = 0;
    const params: Record<string, unknown> = { interval_secs: 2, limit: LIMIT };
    for (const [k, v] of Object.entries(query)) {
      if (v !== undefined && v !== "") params[k] = v;
    }
    const offData = wsClient.subscribe("processes.live", params, (payload) => {
      lastAt.current = Date.now();
      setRows(payload as Proc[]);
      setLoaded(true);
    });
    const offStatus = wsClient.onStatus(setUp);

    const poller = setInterval(() => {
      if (Date.now() - lastAt.current < STALE_SECS * 1000) return;
      void (async () => {
        const { data } = await api.GET("/api/v1/processes", {
          params: { query: { ...query, q: query.q || undefined } },
        });
        // WS 帧到了就让位,别拿旧轮询结果盖新帧
        if (data && Date.now() - lastAt.current >= STALE_SECS * 1000) {
          setRows(data.slice(0, LIMIT));
          setLoaded(true);
        }
      })();
    }, 3_000);

    return () => {
      offData();
      offStatus();
      clearInterval(poller);
    };
  }, [key]);

  return { rows, loaded, up };
}
