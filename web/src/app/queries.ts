import { api } from "@/api/client";
import type { components } from "@/api/schema";

type Capabilities = components["schemas"]["Capabilities"];

/** 机器身份与能力探测。未认证也可访问(system 层),登录门与外壳共用同一份缓存。 */
export function capabilitiesQuery() {
  return {
    queryKey: ["capabilities"],
    queryFn: async (): Promise<Capabilities> => {
      const { data, error } = await api.GET("/api/v1/capabilities");
      if (error) throw error;
      return data;
    },
    staleTime: 60_000,
  } as const;
}

/** 指标快照:外壳用它当「实时」心跳,概览页共用同一份缓存。 */
export function snapshotQuery(enabled: boolean) {
  return {
    queryKey: ["metrics", "current"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/metrics/current");
      if (error) throw error;
      return data;
    },
    refetchInterval: 2_000,
    enabled,
  } as const;
}

export function healthQuery(enabled: boolean) {
  return {
    queryKey: ["system", "health"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/system/health");
      if (error) throw error;
      return data;
    },
    refetchInterval: 30_000,
    enabled,
  } as const;
}
