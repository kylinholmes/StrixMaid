import { useEffect, useState } from "react";
import { api } from "@/api/client";
import type { components } from "@/api/schema";
import { wsClient } from "@/lib/ws";

export type Unit = components["schemas"]["UnitSummary"];
export type Timer = components["schemas"]["TimerEntry"];
export type Scope = "system" | "user";

/** unit 已被服务管理器从内存移除（`services.changed` 的删行约定）。 */
function vanished(u: Unit): boolean {
  return u.load_state === "not_found" && u.active_state === "inactive" && u.sub_state === "dead";
}

export interface UnitsLive {
  rows: readonly Unit[];
  loaded: boolean;
  /** 列表整体拉不下来时的错误文案（如 501 本机没有服务管理器） */
  error: string | null;
}

/**
 * unit 主列表：REST 全量拉一次（只按 scope），之后靠 WS `services.changed`
 * 增量合并;类型/状态/关键字过滤全在客户端做——改筛选不重新取数、不闪屏。
 * WS 只推「变化」，不能当心跳，所以保留一个低频兜底重拉。
 */
export function useUnits(scope: Scope): UnitsLive {
  const [rows, setRows] = useState<readonly Unit[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    setLoaded(false);
    setError(null);

    const fetchAll = async () => {
      const { data, error: err } = await api.GET("/api/v1/services", {
        params: { query: { scope } },
      });
      if (!alive) return;
      if (err) {
        setError((err as { message?: string }).message ?? "服务列表拉取失败");
        setLoaded(true);
        return;
      }
      if (data) {
        setRows(data);
        setError(null);
        setLoaded(true);
      }
    };
    void fetchAll();
    const poller = setInterval(fetchAll, 60_000);

    const offData = wsClient.subscribe("services.changed", {}, (payload) => {
      const changed = payload as Unit[];
      setRows((cur) => {
        const map = new Map(cur.map((u) => [u.name, u]));
        for (const u of changed) {
          if (u.scope !== scope) continue;
          if (vanished(u)) map.delete(u.name);
          else map.set(u.name, u);
        }
        return [...map.values()];
      });
    });

    return () => {
      alive = false;
      clearInterval(poller);
      offData();
    };
  }, [scope]);

  return { rows, loaded, error };
}
