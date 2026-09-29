import { StrictMode, useState } from "react";
import { createRoot } from "react-dom/client";
import { setAuthToken } from "../src/api/client";
import { wsClient } from "../src/lib/ws";
import { useLogs } from "../src/logs/useLogs";

setAuthToken("logs-test");
wsClient.connect("logs-test");

// 测试真实 hook 的渲染和请求生命周期，不替换 React，也不重写分页实现。
function Harness() {
  const [unit, setUnit] = useState("a");
  const logs = useLogs({ unit }, true);
  return (
    <>
      <button type="button" onClick={() => setUnit(unit === "a" ? "b" : "a")}>
        换筛选
      </button>
      <button type="button" onClick={logs.loadOlder}>
        翻页
      </button>
      <button type="button" onClick={() => logs.setAtTop(false)}>
        暂停跟随
      </button>
      <button type="button" onClick={logs.flush}>
        回到最新
      </button>
      <output data-testid="state">
        {JSON.stringify({
          loaded: logs.loaded,
          busy: logs.loadingOlder,
          hasMore: logs.hasMore,
          pending: logs.pending,
          truncated: logs.pendingTruncated,
          error: logs.error,
          count: logs.rows.length,
          first: logs.rows[0]?.cursor,
          last: logs.rows.at(-1)?.cursor,
        })}
      </output>
      <output data-testid="messages">{logs.rows.map((row) => row.message).join("\n")}</output>
    </>
  );
}
createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <Harness />
  </StrictMode>,
);
