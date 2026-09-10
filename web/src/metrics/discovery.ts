import { useQuery } from "@tanstack/react-query";
import { api } from "@/api/client";
import type { components } from "@/api/schema";
import { useSession } from "@/session/useSession";

type SeriesMeta = components["schemas"]["SeriesMeta"];

/** 从 `k=v,k2=v2` 里取一个标签值。 */
export function labelValue(labels: string, key: string): string | null {
  for (const part of labels.split(",")) {
    const [k, v] = part.split("=");
    if (k === key && v) return v;
  }
  return null;
}

export interface Discovery {
  /** 全部序列 */
  all: readonly SeriesMeta[];
  /** 有任意此前缀的序列 */
  has: (prefix: string) => boolean;
  /** 某指标下按标签键去重后的成员列表（如 disk.util 的 dev 们），排序稳定 */
  members: (metric: string, labelKey: string) => string[];
}

/**
 * 序列发现：页面结构的唯一依据（roadmap/08 §6.1 的退化规则）。
 * 探测不到的资源整类隐藏；成员清单从标签里来，不硬编码。
 */
export function useDiscovery(): { data: Discovery | undefined; isPending: boolean } {
  const open = useSession((st) => st.status) === "open";
  const q = useQuery({
    queryKey: ["metrics", "series"],
    queryFn: async (): Promise<readonly SeriesMeta[]> => {
      const { data, error } = await api.GET("/api/v1/metrics/series", { params: { query: {} } });
      if (error) throw error;
      return data;
    },
    staleTime: 60_000,
    enabled: open,
  });

  const all = q.data;
  return {
    isPending: q.isPending,
    data: all
      ? {
          all,
          has: (prefix) => all.some((m) => m.metric.startsWith(prefix)),
          members: (metric, labelKey) => {
            const out = new Set<string>();
            for (const m of all) {
              if (m.metric !== metric) continue;
              const v = labelValue(m.labels, labelKey);
              if (v) out.add(v);
            }
            return [...out].sort((a, b) => a.localeCompare(b, undefined, { numeric: true }));
          },
        }
      : undefined,
  };
}
