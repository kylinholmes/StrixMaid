import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "@/api/client";
import type { components } from "@/api/schema";
import { wsClient } from "@/lib/ws";
import { sessionSignal } from "@/session/lifecycle";
import { LOG_CAPACITY, prependBounded } from "./buffer";

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
const MAX_ENTRIES = LOG_CAPACITY;

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
  /** 等待期间有日志移出缓冲，回到最新后可通过游标继续翻页。 */
  pendingTruncated: boolean;
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
  const [pendingTruncated, setPendingTruncated] = useState(false);
  const overflow = useRef(false);

  const nextCursor = useRef<string | null>(null);
  const atTop = useRef(true);
  /** 由新到旧攒着的 follow 条目 */
  const buf = useRef<LogEntry[]>([]);
  // 每轮筛选拥有自己的请求域。游标可以跨筛选重复，不能拿它当请求身份。
  const requests = useRef<AbortController | null>(null);
  const older = useRef<AbortController | null>(null);
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
    const controller = new AbortController();
    requests.current = controller;
    const signal = AbortSignal.any([controller.signal, sessionSignal()]);
    older.current?.abort();
    older.current = null;
    setLoadingOlder(false);
    setHasMore(false);
    atTop.current = true;
    setLoaded(false);
    setError(null);
    setRows([]);
    setPending(0);
    setPendingTruncated(false);
    buf.current = [];
    overflow.current = false;
    nextCursor.current = null;
    void (async () => {
      const { data, error: err } = await api.GET("/api/v1/logs", {
        params: { query: { ...filterRef.current, limit: PAGE } },
        signal,
      });
      if (signal.aborted) return;
      if (err) {
        setError((err as { message?: string }).message ?? "日志查询失败");
      } else if (data) {
        nextCursor.current = data.next_cursor ?? null;
        setHasMore(!!data.next_cursor);
        // REST 首页与 follow 同时启动。保留等待期间已收到的日志，并按游标去重。
        setRows((current) => {
          const seen = new Set(current.map((row) => row.cursor));
          const merged = [
            ...current,
            ...(data.entries ?? []).filter((row) => !seen.has(row.cursor)),
          ];
          merged.sort((a, b) => b.ts - a.ts || b.us - a.us);
          return trim(merged);
        });
      }
      setLoaded(true);
    })().catch(() => {
      if (!signal.aborted) {
        setError("日志连接中断，请重试");
        setLoaded(true);
      }
    });
    return () => {
      controller.abort();
      older.current?.abort();
    };
  }, [key, trim]);

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
        const appended = prependBounded(newest, buf.current);
        buf.current = appended.rows;
        if (appended.dropped > 0) {
          overflow.current = true;
          setPendingTruncated(true);
        }
        setPending(buf.current.length);
      }
    });
    return off;
  }, [key, follow, trim]);

  const loadOlder = useCallback(() => {
    const cursor = nextCursor.current;
    const owner = requests.current;
    if (!cursor || !owner || owner.signal.aborted || older.current) return;
    // 请求只在事件入口发起，不能放进 React state updater（StrictMode 会重复调用）。
    const request = new AbortController();
    older.current = request;
    const signal = AbortSignal.any([request.signal, owner.signal, sessionSignal()]);
    setLoadingOlder(true);
    void (async () => {
      try {
        const { data, error: err } = await api.GET("/api/v1/logs", {
          params: { query: { ...filterRef.current, cursor, limit: PAGE } },
          signal,
        });
        if (signal.aborted || nextCursor.current !== cursor) return;
        if (err) {
          setError(err.message ?? "日志查询失败");
        } else if (data) {
          setRows((cur) => [...cur, ...(data.entries ?? [])].slice(-MAX_ENTRIES));
          nextCursor.current = data.next_cursor ?? null;
          setHasMore(!!data.next_cursor);
        }
      } catch {
        if (!signal.aborted) setError("日志连接中断，请重试");
      } finally {
        if (older.current === request) {
          older.current = null;
          if (!signal.aborted) setLoadingOlder(false);
        }
      }
    })();
  }, []);

  const flush = useCallback(() => {
    older.current?.abort();
    older.current = null;
    setLoadingOlder(false);
    const newest = buf.current;
    buf.current = [];
    setPending(0);
    setPendingTruncated(false);
    if (newest.length > 0) {
      if (overflow.current) {
        nextCursor.current = newest.at(-1)?.cursor ?? null;
        setHasMore(nextCursor.current !== null);
        setRows(newest);
      } else {
        setRows((cur) => trim([...newest, ...cur]));
      }
    }
    overflow.current = false;
  }, [trim]);

  const setAtTop = useCallback(
    (v: boolean) => {
      atTop.current = v;
      // 回到顶部即视为「已读到最新」,自动合并
      if (v && buf.current.length > 0) flush();
    },
    [flush],
  );

  return {
    rows,
    loaded,
    error,
    hasMore,
    loadingOlder,
    loadOlder,
    pending,
    pendingTruncated,
    flush,
    setAtTop,
  };
}
