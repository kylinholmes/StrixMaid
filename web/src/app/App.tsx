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

/** 发行版主题联动：capabilities 的 identity 一到就把 accent 切到这台机器的发行版。 */
function DistroSync() {
  const caps = useQuery(capabilitiesQuery());
  const setDistroById = useTheme((t) => t.setDistroById);
  const osId = caps.data?.identity?.os_id;
  useEffect(() => {
    if (osId !== undefined) setDistroById(osId);
  }, [osId, setDistroById]);
  return null;
}

export function App() {
  const restore = useSession((st) => st.restore);
  useEffect(() => {
    void restore();
  }, [restore]);

  return (
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>
        <DistroSync />
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
            <Route path="/terminal" element={<StubPage title="终端" />} />
            <Route path="/files" element={<StubPage title="文件" />} />
            <Route path="/audit" element={<StubPage title="审计" />} />
            <Route path="/settings" element={<StubPage title="设置" />} />
            <Route path="*" element={<Navigate to="/overview" replace />} />
          </Route>
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
