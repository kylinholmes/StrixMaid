import { ChevronDown, ChevronUp, Plus, X } from "lucide-react";
import { useCallback, useRef } from "react";
import { api } from "@/api/client";
import { Button } from "@/components";
import { cx } from "@/lib/cx";
import { atTabLimit, MAX_TABS, useWorkspace } from "./store";
import { TerminalTab } from "./TerminalTab";
import s from "./Workspace.module.css";

/** shell 路径 → 标签名（`/bin/zsh` → `zsh`，`C:\...\cmd.exe` → `cmd.exe`）。 */
function titleOf(shell: string): string {
  const cut = Math.max(shell.lastIndexOf("/"), shell.lastIndexOf("\\"));
  return cut >= 0 ? shell.slice(cut + 1) : shell;
}

/**
 * 终端面板（§4.1、§4.3）：多标签 + `+` 新建 + 上边缘拖高 + 折叠成底栏。
 *
 * - 每个标签一条 WS，切走的标签保持连接（TerminalTab 不卸载，只 CSS 隐藏）；
 * - `+` 在到达每会话上限（8）时禁用，UI 自己算，不等后端 409；
 * - 关闭 live/断线标签先 DELETE（关掉 PTY），已退出的标签只从列表移除
 *   ——终端本身已经没了，再发 DELETE 只会得到 404。
 */
export function TerminalPanel() {
  const tabs = useWorkspace((st) => st.tabs);
  const activeId = useWorkspace((st) => st.activeId);
  const collapsed = useWorkspace((st) => st.panelCollapsed);
  const height = useWorkspace((st) => st.panelHeight);
  const addTab = useWorkspace((st) => st.addTab);
  const removeTab = useWorkspace((st) => st.removeTab);
  const setActive = useWorkspace((st) => st.setActive);
  const setPanelHeight = useWorkspace((st) => st.setPanelHeight);
  const setPanelCollapsed = useWorkspace((st) => st.setPanelCollapsed);
  const limit = useWorkspace(atTabLimit);
  const creating = useRef(false);

  const create = useCallback(async () => {
    // 连点保护：POST 在途时不再发第二个。
    if (creating.current) return;
    creating.current = true;
    try {
      const { data, error } = await api.POST("/api/v1/terminals", { body: {} });
      if (error || !data) return;
      // 列表接口才有 shell 等元数据；此处用默认名，附着后标题无关紧要。
      const { data: list } = await api.GET("/api/v1/terminals");
      const info = list?.find((t) => t.id === data.id);
      addTab({ id: data.id, title: info ? titleOf(info.shell) : "shell", status: "live" });
      setPanelCollapsed(false);
    } finally {
      creating.current = false;
    }
  }, [addTab, setPanelCollapsed]);

  const close = useCallback(
    (id: string) => {
      const tab = useWorkspace.getState().tabs.find((t) => t.id === id);
      // 已退出的终端后端已经不在了；对活着的先关 PTY。失败也移除标签：
      // 404 说明它本来就没了，别的错误由空闲回收兜底。
      if (tab && tab.status !== "exited") {
        void api.DELETE("/api/v1/terminals/{id}", { params: { path: { id } } });
      }
      removeTab(id);
    },
    [removeTab],
  );

  /** 上边缘拖高：指针事件 + 全局 move/up，松手为止。 */
  const dragStart = useCallback(
    (e: React.PointerEvent) => {
      const startY = e.clientY;
      const startH = useWorkspace.getState().panelHeight;
      const move = (ev: PointerEvent) => setPanelHeight(startH + (startY - ev.clientY));
      const up = () => {
        window.removeEventListener("pointermove", move);
        window.removeEventListener("pointerup", up);
      };
      window.addEventListener("pointermove", move);
      window.addEventListener("pointerup", up);
    },
    [setPanelHeight],
  );

  return (
    <section
      className={cx(s.panel, collapsed && s.panelCollapsed)}
      style={collapsed ? undefined : { height }}
      aria-label="终端面板"
    >
      {!collapsed && (
        // biome-ignore lint/a11y/useSemanticElements: 这是可拖动的窗口分隔条（focusable separator），<hr> 表达不了交互
        <div
          className={s.dragHandle}
          onPointerDown={dragStart}
          role="separator"
          aria-orientation="horizontal"
          aria-label="调整终端面板高度"
          aria-valuenow={height}
          tabIndex={0}
          onKeyDown={(e) => {
            if (e.key === "ArrowUp") setPanelHeight(height + 24);
            if (e.key === "ArrowDown") setPanelHeight(height - 24);
          }}
        />
      )}
      <div className={s.tabBar}>
        {tabs.map((t) => (
          <div
            key={t.id}
            className={cx(
              s.tab,
              t.id === activeId && s.tabActive,
              t.status === "exited" && s.tabExited,
            )}
          >
            <button
              type="button"
              className={s.tabSelect}
              onClick={() => {
                setActive(t.id);
                setPanelCollapsed(false);
              }}
            >
              <span>{t.status === "exited" ? (t.exitLabel ?? "已退出") : t.title}</span>
              {t.status === "disconnected" && <span title="连接断开">⚡</span>}
            </button>
            <button
              type="button"
              className={s.tabClose}
              aria-label={`关闭 ${t.title}`}
              onClick={() => close(t.id)}
            >
              <X size={12} />
            </button>
          </div>
        ))}
        <Button
          iconOnly
          size="sm"
          aria-label="新建终端"
          disabled={limit}
          title={limit ? `本会话已达 ${MAX_TABS} 个终端上限` : "新建终端"}
          onClick={() => void create()}
        >
          <Plus size={14} />
        </Button>
        <span style={{ flex: 1 }} />
        <Button
          iconOnly
          size="sm"
          aria-label={collapsed ? "展开终端面板" : "折叠终端面板"}
          onClick={() => setPanelCollapsed(!collapsed)}
        >
          {collapsed ? <ChevronUp size={14} /> : <ChevronDown size={14} />}
        </Button>
      </div>
      {/* 折叠只是藏起来，不卸载——卸载会断掉每个标签的 WS（§4.3：保持连接）。 */}
      <div className={cx(s.terminals, collapsed && s.terminalsHidden)}>
        {tabs.length === 0 ? (
          <div className={s.panelEmpty}>
            <span>还没有终端。</span>
            <Button size="sm" onClick={() => void create()}>
              开一个
            </Button>
          </div>
        ) : (
          tabs.map((t) => <TerminalTab key={t.id} id={t.id} active={t.id === activeId} />)
        )}
      </div>
    </section>
  );
}
