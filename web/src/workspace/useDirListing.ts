import { keepPreviousData, useQuery, useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect } from "react";
import { api } from "@/api/client";
import type { components } from "@/api/schema";

export type DirListing = components["schemas"]["DirListing"];

/**
 * 取一个目录的列表，附带 §4.8 的两条刷新路：
 *
 * 1. **焦点刷新**：窗口重新获得焦点 / 标签页重新可见时自动重取。目录变化通常
 *    正是使用者自己在下面的终端里造成的，他做完就会看向文件区；
 * 2. **手动刷新**：`refresh()`，给一个位置固定的按钮用。
 *
 * 不做推送（inotify 三平台三套实现，代价与收益不匹配——§4.8 的决定）。
 *
 * `placeholderData: keepPreviousData`：刷新与导航时旧列表留在原地，配
 * `ProgressLine` 表示「正在刷新」，而不是闪一下骨架屏——这也是「刷新保住
 * 滚动位置」的一半（另一半是列表容器不重挂）。
 */
export function useDirListing(path: string | null) {
  const qc = useQueryClient();
  const query = useQuery({
    queryKey: ["dir", path],
    enabled: path !== null,
    placeholderData: keepPreviousData,
    queryFn: async (): Promise<DirListing> => {
      if (path === null) throw new Error("unreachable");
      const { data, error } = await api.GET("/api/v1/files", {
        params: { query: { path } },
      });
      if (error) throw error;
      return data;
    },
  });

  const refresh = useCallback(() => {
    if (path !== null) void qc.invalidateQueries({ queryKey: ["dir", path] });
  }, [qc, path]);

  useEffect(() => {
    if (path === null) return;
    const onFocus = () => {
      if (document.visibilityState === "visible") refresh();
    };
    window.addEventListener("focus", onFocus);
    document.addEventListener("visibilitychange", onFocus);
    return () => {
      window.removeEventListener("focus", onFocus);
      document.removeEventListener("visibilitychange", onFocus);
    };
  }, [path, refresh]);

  return { ...query, refresh };
}
