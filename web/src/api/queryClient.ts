import { QueryClient } from "@tanstack/react-query";
import { onSessionReset } from "@/session/lifecycle";

export const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: 1, refetchOnWindowFocus: false } },
});

// clear 同时取消旧查询，已取消请求的迟到结果不能重新填充缓存。
onSessionReset(() => queryClient.clear());
