import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { StrictMode, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { setAuthToken } from "../src/api/client";
import { onSessionReset } from "../src/session/lifecycle";
import { FileList } from "../src/workspace/FileList";
import { FilePreview, type PreviewTarget } from "../src/workspace/preview/FilePreview";
import "../src/styles/tokens.css";
import "../src/styles/base.css";
import { applyTheme, useTheme } from "../src/theme/useTheme";

const { theme, distro } = useTheme.getState();
applyTheme("dark", theme, distro);
setAuthToken("preview-test");
const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
function Harness() {
  const [path, setPath] = useState("/fixtures");
  const [direct, setDirect] = useState<PreviewTarget | null>(null);
  useEffect(() => {
    const switchFile = (event: Event) => setDirect((event as CustomEvent<PreviewTarget>).detail);
    const reset = () => setAuthToken("new-session");
    window.addEventListener("preview:switch", switchFile);
    window.addEventListener("preview:reset", reset);
    const off = onSessionReset(() => setDirect(null));
    return () => {
      off();
      window.removeEventListener("preview:switch", switchFile);
      window.removeEventListener("preview:reset", reset);
    };
  }, []);
  return (
    <main style={{ height: "90vh", display: "flex" }}>
      <FileList
        path={path}
        platform="unix"
        onNavigate={setPath}
        canBack={false}
        canForward={false}
        onBack={() => {}}
        onForward={() => {}}
      />
      {direct && <FilePreview target={direct} onClose={() => setDirect(null)} />}
    </main>
  );
}
createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={client}>
      <Harness />
    </QueryClientProvider>
  </StrictMode>,
);
