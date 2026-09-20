import { keepPreviousData, useInfiniteQuery, useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo } from "react";
import { api } from "@/api/client";
import type { components } from "@/api/schema";

export type DirListing = components["schemas"]["DirListing"];
export type DirEntry = components["schemas"]["DirEntryInfo"];
export type FileSortKey = components["schemas"]["FileSortKey"];

/** 一页多少条（roadmap/12 §4.5 的后端分页）。普通目录一页装完，与旧行为无异。 */
export const PAGE_SIZE = 500;

export interface DirSort {
  key: FileSortKey;
  desc: boolean;
}

/**
 * 分页取一个目录，附带 §4.8 的两条刷新路：
 *
 * 1. **焦点刷新**：窗口重新获得焦点 / 标签页重新可见时自动重取（300ms 去重
 *    ——两个事件几毫秒内先后触发，各刷一次等于把在途请求取消重发）；
 * 2. **手动刷新**：`refresh()`。
 *
 * 排序在**服务端**做（`sort`/`order`），分页在排序之后切——客户端排序在
 * 分页面前是错的：本页排得再好也只是全量的一个错误切片。
 */
export function useDirListing(path: string | null, sort: DirSort) {
  const qc = useQueryClient();
  const queryKey = ["dir", path, sort.key, sort.desc] as const;

  const query = useInfiniteQuery({
    queryKey,
    enabled: path !== null,
    placeholderData: keepPreviousData,
    initialPageParam: 0,
    queryFn: async ({ pageParam }): Promise<DirListing> => {
      if (path === null) throw new Error("unreachable");
      const { data, error } = await api.GET("/api/v1/files", {
        params: {
          query: {
            path,
            limit: PAGE_SIZE,
            offset: pageParam,
            sort: sort.key,
            order: sort.desc ? "desc" : "asc",
          },
        },
      });
      if (error) throw error;
      return data;
    },
    getNextPageParam: (last, pages) => {
      const loaded = pages.reduce((n, p) => n + (p.entries?.length ?? 0), 0);
      // 老服务端没有 total（也不分页）：一页即全部。
      if (last.total === undefined || last.total === null) return undefined;
      return loaded < last.total ? loaded : undefined;
    },
  });

  const refresh = useCallback(() => {
    if (path !== null) void qc.invalidateQueries({ queryKey: ["dir", path] });
  }, [qc, path]);

  useEffect(() => {
    if (path === null) return;
    let last = 0;
    const onFocus = () => {
      if (document.visibilityState !== "visible") return;
      const now = Date.now();
      if (now - last < 300) return;
      last = now;
      refresh();
    };
    window.addEventListener("focus", onFocus);
    document.addEventListener("visibilitychange", onFocus);
    return () => {
      window.removeEventListener("focus", onFocus);
      document.removeEventListener("visibilitychange", onFocus);
    };
  }, [path, refresh]);

  const entries = useMemo(
    () => (query.data?.pages ?? []).flatMap((p) => p.entries ?? []),
    [query.data],
  );
  const firstPage = query.data?.pages[0];
  const total = firstPage?.total ?? entries.length;
  const skipped = firstPage?.skipped ?? 0;

  return {
    entries,
    total,
    skipped,
    isPending: query.isPending,
    isFetching: query.isFetching,
    error: query.error,
    hasNextPage: query.hasNextPage,
    isFetchingNextPage: query.isFetchingNextPage,
    fetchNextPage: query.fetchNextPage,
    refresh,
  };
}
