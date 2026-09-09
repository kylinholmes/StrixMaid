import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "@/api/client";
import type { components } from "@/api/schema";
import { wsClient } from "@/lib/ws";

export type LogEntry = components["schemas"]["LogEntry"];
export type LogPriority = components["schemas"]["LogPriority"];

export interface LogFilter {
  /** 最低级别(严重程度 >= 该值) */
  priority?: LogPriority;
  unit?: string;
  boot?: string;
  q?: string;
  /** 起始时刻,unix 秒。macOS 缺省回看 5 分钟(后端约束) */
  since?: number;
}

/** 每页条数。 */
const PAGE = 200;
/** 内存里最多留多少条:超出就裁掉最旧的,游标退回被裁处,翻页可再取。 */
const MAX_ENTRIES = 5000;

export interface LogsLive {
  /** 由新到旧 */
  rows: readonly LogEntry[];
  loaded: boolean;
  error: string | null;
  hasMore: boolean;
  loadingOlder: boolean;
  /** 向更旧方向翻一页(滚到底时表格调用) */
  loadOlder: () => void;
  /** 跟随暂停期间攒下的新日志条数 */
  pending: number;
  /** 合并攒下的新日志(「回到最新」) */
  flush: () => void;
  /** 表格滚动回调:视口是否贴着顶部。贴顶时新日志直接进表,否则攒着 */
  setAtTop: (v: boolean) => void;
}

/**
 * 日志数据层:REST 拉第一页,游标向更旧翻(内容分片),WS `logs.follow` 实时插顶。
 * 用户滚离顶部时新条目进 pending 缓冲,避免正在读的行被顶走。
 */
export function useLogs(filter: LogFilter, follow: boolean): LogsLive {
  const [rows, setRows] = useState<readonly LogEntry[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [pending, setPending] = useState(0);

  const nextCursor = useRef<string | null>(null);
  const atTop = useRef(true);
  /** 由新到旧攒着的 follow 条目 */
  const buf = useRef<LogEntry[]>([]);
  const filterRef = useRef(filter);
  filterRef.current = filter;
  const key = JSON.stringify(filter);

  /** 裁掉超出内存上限的最旧部分,游标退回被裁处。 */
  const trim = useCallback((list: LogEntry[]): LogEntry[] => {
    if (list.length <= MAX_ENTRIES) return list;
    const kept = list.slice(0, MAX_ENTRIES);
    nextCursor.current = kept[kept.length - 1]?.cursor ?? null;
    setHasMore(true);
    return kept;
  }, []);

  // 首页
  // biome-ignore lint/correctness/useExhaustiveDependencies: key 串已覆盖 filter 全部字段
  useEffect(() => {
    let alive = true;
    setLoaded(false);
    setError(null);
    setRows([]);
    setPending(0);
    buf.current = [];
    nextCursor.current = null;
    void (async () => {
      const { data, error: err } = await api.GET("/api/v1/logs", {
        params: { query: { ...filterRef.current, limit: PAGE } },
      });
      if (!alive) return;
      if (err) {
        setError((err as { message?: string }).message ?? "日志查询失败");
      } else if (data) {
        setRows(data.entries ?? []);
        nextCursor.current = data.next_cursor ?? null;
        setHasMore(!!data.next_cursor);
      }
      setLoaded(true);
    })();
    return () => {
      alive = false;
    };
  }, [key]);

  // 跟随:since/cursor 对 follow 无意义,订阅参数只带过滤器
  // biome-ignore lint/correctness/useExhaustiveDependencies: key 串已覆盖 filter 全部字段
  useEffect(() => {
    if (!follow) return;
    const f = filterRef.current;
    const params: Record<string, unknown> = {};
    if (f.priority) params.priority = f.priority;
    if (f.unit) params.unit = f.unit;
    if (f.boot) params.boot = f.boot;
    if (f.q) params.q = f.q;
    const off = wsClient.subscribe("logs.follow", params, (payload) => {
      // 帧内按时间升序,插顶要反过来(整表由新到旧)
      const newest = [...(payload as LogEntry[])].reverse();
      if (newest.length === 0) return;
      if (atTop.current && buf.current.length === 0) {
        setRows((cur) => trim([...newest, ...cur]));
      } else {
        buf.current = [...newest, ...buf.current];
        setPending(buf.current.length);
      }
    });
    return off;
  }, [key, follow, trim]);

  const loadOlder = useCallback(() => {
    const cursor = nextCursor.current;
    if (!cursor) return;
    setLoadingOlder((busy) => {
      if (busy) return busy;
      void (async () => {
        const { data } = await api.GET("/api/v1/logs", {
          params: { query: { ...filterRef.current, cursor, limit: PAGE } },
        });
        // 期间筛选变了就丢弃这页(新 effect 已经重置了列表)
        if (data && nextCursor.current === cursor) {
          setRows((cur) => [...cur, ...(data.entries ?? [])]);
          nextCursor.current = data.next_cursor ?? null;
          setHasMore(!!data.next_cursor);
        }
        setLoadingOlder(false);
      })();
      return true;
    });
  }, []);

  const flush = useCallback(() => {
    const newest = buf.current;
    buf.current = [];
    setPending(0);
    if (newest.length > 0) setRows((cur) => trim([...newest, ...cur]));
  }, [trim]);

  const setAtTop = useCallback(
    (v: boolean) => {
      atTop.current = v;
      // 回到顶部即视为「已读到最新」,自动合并
      if (v && buf.current.length > 0) flush();
    },
    [flush],
  );

  return { rows, loaded, error, hasMore, loadingOlder, loadOlder, pending, flush, setAtTop };
}
