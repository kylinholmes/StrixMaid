import { QueryClient, QueryClientProvider, useQuery } from "@tanstack/react-query";
import { useEffect } from "react";
import { BrowserRouter, Navigate, Route, Routes } from "react-router-dom";
import { Gallery } from "@/gallery/Gallery";
import { LogsPage } from "@/logs/LogsPage";
import { PerfPage } from "@/perf/PerfPage";
import { ProcPage } from "@/proc/ProcPage";
import { useSession } from "@/session/useSession";
import { SvcPage } from "@/svc/SvcPage";
import { useTheme } from "@/theme/useTheme";
import { Workspace } from "@/workspace/Workspace";
import { Overview } from "./Overview";
import { capabilitiesQuery } from "./queries";
import { Shell } from "./Shell";
import { StubPage } from "./StubPage";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 1,
      refetchOnWindowFocus: false,
    },
  },
});

/**
 * 平台联动：capabilities 的 identity 一到就把设计语言与发行版身份都切到这台机器。
 *
 * 传的是 `PlatformIdentity` 而不是裸的 id，将来后端加上桌面环境时
 * 只在这里多带一个字段，`pickTheme` 那边不用改调用形状。
 */
function PlatformSync() {
  const caps = useQuery(capabilitiesQuery());
  const setPlatform = useTheme((t) => t.setPlatform);
  const osId = caps.data?.identity?.os_id;
  useEffect(() => {
    if (osId !== undefined) setPlatform({ osId });
  }, [osId, setPlatform]);
  return null;
}

export function App() {
  const restore = useSession((st) => st.restore);
  useEffect(() => {
    void restore();
  }, [restore]);

  // 桌面应用手感：右键不弹浏览器菜单。输入区与终端放行——粘贴等原生
  // 菜单在那里是刚需（xterm 的复制粘贴走浏览器菜单）。
  useEffect(() => {
    const onMenu = (e: MouseEvent) => {
      const t = e.target as HTMLElement | null;
      if (t?.closest('input, textarea, [contenteditable="true"], .xterm')) return;
      e.preventDefault();
    };
    document.addEventListener("contextmenu", onMenu);
    return () => document.removeEventListener("contextmenu", onMenu);
  }, []);

  return (
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>
        <PlatformSync />
        <Routes>
          {/* 组件合集页保留为开发入口，不进导航 */}
          <Route path="/gallery" element={<Gallery />} />
          <Route element={<Shell />}>
            <Route index element={<Navigate to="/overview" replace />} />
            <Route path="/overview" element={<Overview />} />
            <Route path="/performance" element={<PerfPage />} />
            <Route path="/performance/:res" element={<PerfPage />} />
            <Route path="/performance/:res/:member" element={<PerfPage />} />
            <Route path="/processes" element={<ProcPage />} />
            <Route path="/services" element={<SvcPage />} />
            <Route path="/logs" element={<LogsPage />} />
            {/* 两个入口指向同一个工作区，只是初始状态不同（roadmap/12 §4.1）。 */}
            <Route path="/terminal" element={<Workspace initial="terminal" />} />
            <Route path="/files" element={<Workspace initial="files" />} />
            <Route path="/audit" element={<StubPage title="审计" />} />
            <Route path="/settings" element={<StubPage title="设置" />} />
            <Route path="*" element={<Navigate to="/overview" replace />} />
          </Route>
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
