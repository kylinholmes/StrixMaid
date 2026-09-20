import { useQuery } from "@tanstack/react-query";
import { ChevronDown, ChevronsUpDown, ChevronUp, Plus, X } from "lucide-react";
import { useCallback, useRef, useState } from "react";
import { api } from "@/api/client";
import { Button, Menu } from "@/components";
import { cx } from "@/lib/cx";
import { useDismiss } from "@/lib/useDismiss";
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
  /** 上一次开终端失败的原因。开成功或再点一次时清掉。 */
  const [createError, setCreateError] = useState<string | null>(null);
  const [shellMenu, setShellMenu] = useDismiss();
  // 清单几乎不变，缓存一小时；面板挂载即取，免得第一次点下拉要等。
  const shells = useQuery({
    queryKey: ["terminals", "shells"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/terminals/shells");
      if (error) throw error;
      return data;
    },
    staleTime: 3_600_000,
  });

  const create = useCallback(
    async (shell?: string) => {
      // 连点保护：POST 在途时不再发第二个。
      if (creating.current) return;
      creating.current = true;
      setCreateError(null);
      try {
        const { data, error } = await api.POST("/api/v1/terminals", {
          body: shell ? { shell } : {},
        });
        if (error || !data) {
          // 静默失败会让人对着一个没反应的按钮连点：本页数不到的 409
          // （另一个窗口占着同一会话的名额）、5xx、断网都要说出来。
          setCreateError(
            (error as { message?: string } | undefined)?.message ?? "开终端失败，请重试",
          );
          return;
        }
        // 列表接口才有 shell 等元数据；此处用默认名，附着后标题无关紧要。
        const { data: list } = await api.GET("/api/v1/terminals");
        const info = list?.find((t) => t.id === data.id);
        addTab({ id: data.id, title: info ? titleOf(info.shell) : "shell", status: "live" });
        setPanelCollapsed(false);
      } finally {
        creating.current = false;
      }
    },
    [addTab, setPanelCollapsed],
  );

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

  /**
   * 上边缘拖高。用指针捕获而不是 window 监听：光标移出窗口再松手时
   * window 收不到 pointerup，监听器漏在那里，面板会一直粘着光标；
   * 捕获保证 move/up/cancel 都送到把手上，任一结束路径都能拆干净。
   */
  const dragStart = useCallback(
    (e: React.PointerEvent<HTMLDivElement>) => {
      const el = e.currentTarget;
      el.setPointerCapture(e.pointerId);
      const startY = e.clientY;
      const startH = useWorkspace.getState().panelHeight;
      const move = (ev: PointerEvent) => setPanelHeight(startH + (startY - ev.clientY));
      const end = () => {
        el.removeEventListener("pointermove", move);
        el.removeEventListener("pointerup", end);
        el.removeEventListener("pointercancel", end);
      };
      el.addEventListener("pointermove", move);
      el.addEventListener("pointerup", end);
      el.addEventListener("pointercancel", end);
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
      {/* 菜单挂在面板层而不是标签栏里：标签栏 overflow-x:auto 会把向上弹出的
          菜单裁掉/挡住点击。从面板上边缘向上弹，盖在文件区之上。 */}
      {shellMenu && (
        <Menu
          label="选择 shell"
          style={{ position: "absolute", bottom: "100%", left: "var(--sp-3)", zIndex: 10 }}
          items={(shells.data ?? []).map((sh) => ({
            id: sh.path,
            label: sh.default ? `${sh.name}（默认）` : sh.name,
          }))}
          onPick={(path) => {
            setShellMenu(false);
            void create(path);
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
          title={limit ? `本会话已达 ${MAX_TABS} 个终端上限` : "新建终端（默认 shell）"}
          onClick={() => void create()}
        >
          <Plus size={14} />
        </Button>
        <Button
          iconOnly
          size="sm"
          aria-label="选择 shell 新建终端"
          disabled={limit}
          onClick={(e) => {
            e.stopPropagation();
            setShellMenu((v) => !v);
          }}
        >
          <ChevronsUpDown size={13} />
        </Button>
        {createError && (
          <span className={s.createError} role="alert">
            {createError}
          </span>
        )}
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
