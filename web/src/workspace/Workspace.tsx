import { useQuery } from "@tanstack/react-query";
import { PanelLeft } from "lucide-react";
import { useCallback, useEffect, useRef } from "react";
import { useLocation } from "react-router-dom";
import { capabilitiesQuery } from "@/app/queries";
import { Button } from "@/components";
import { cx } from "@/lib/cx";
import { useSession } from "@/session/useSession";
import { buildCdBytes } from "./cwd";
import { FileList } from "./FileList";
import { guessHome, platformOf } from "./path";
import { QuickAccess } from "./QuickAccess";
import { useWorkspace } from "./store";
import { TerminalPanel } from "./TerminalPanel";
import { termSockets } from "./termsocket";
import s from "./Workspace.module.css";

export interface WorkspaceProps {
  /** 从哪个入口进来：`terminal` → 终端面板展开；`files` → 收起（§4.1）。 */
  initial: "terminal" | "files";
}

/**
 * 工作区（`docs/roadmap/12-workspace.md`）：终端与文件合成一块。
 * 「我在哪个目录」是一个状态，不是两个——B 期先把两半放进同一个页面，
 * cwd 双向联动（OSC 7）是 C 期。
 */
export function Workspace({ initial }: WorkspaceProps) {
  const cwd = useWorkspace((st) => st.cwd);
  const setCwd = useWorkspace((st) => st.setCwd);
  const railCollapsed = useWorkspace((st) => st.railCollapsed);
  const toggleRail = useWorkspace((st) => st.toggleRail);
  const setPanelCollapsed = useWorkspace((st) => st.setPanelCollapsed);

  const caps = useQuery(capabilitiesQuery());
  const user = useSession((st) => st.user);
  const osId = caps.data?.identity?.os_id;
  const platform = platformOf(osId);

  // 入口决定面板状态。**每次导航都重新生效**（key 于每次导航变化）：
  // React Router 在 /terminal ↔ /files 之间切换时复用同一个组件实例，
  // 只看「首次挂载」的话，点导航里的「终端」什么也不会发生。
  const { key: navKey } = useLocation();
  // biome-ignore lint/correctness/useExhaustiveDependencies: navKey 就是要的触发器——每次导航（含同路径重复点击）重新应用入口初始态
  useEffect(() => {
    setPanelCollapsed(initial === "files");
  }, [initial, navKey, setPanelCollapsed]);

  // ⌃` 切换终端面板（VSCode 同款）。capture 拦在 xterm 之前，终端聚焦时也能收。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.ctrlKey && !e.metaKey && !e.altKey && e.code === "Backquote") {
        e.preventDefault();
        e.stopPropagation();
        const st = useWorkspace.getState();
        st.setPanelCollapsed(!st.panelCollapsed);
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, []);

  // 起点：主目录（capabilities 与会话就绪后才推得出）。
  useEffect(() => {
    if (cwd === null && user && caps.data) {
      setCwd(guessHome(osId, user.username, user.uid));
    }
  }, [cwd, user, caps.data, osId, setCwd]);

  // 导航历史：所有**用户在文件区发起**的导航都过 navigate，后退/前进才知道
  // 来龙去脉。终端驱动的跟随不进历史（cd 十次不该产生十个后退步）。
  const past = useRef<string[]>([]);
  const future = useRef<string[]>([]);
  /**
   * 反向 cd 之后，流里可能还有一个 cd **之前**的 OSC 7 在路上，它报的是旧
   * 目录；照单全收会把文件区拽回去。挡板按**事件序**工作、不用时钟：只在
   * 真的发出过 cd 时记下（标签, 旧目录），该标签的下一个报告等于旧目录 →
   * 忽略；是任何别的值（含目标）→ 清除挡板并照常跟随。我们注入的 cd 必然
   * 在下一个提示符产生报告，挡板必然被清，不存在「真 cd 被误伤」的窗口。
   */
  const staleGuard = useRef<{ tabId: string | null; prev: string | null }>({
    tabId: null,
    prev: null,
  });
  const navigate = useCallback(
    (path: string) => {
      const cur = useWorkspace.getState().cwd;
      if (cur === path) return;
      if (cur !== null) past.current.push(cur);
      future.current = [];
      setCwd(path);
      // 反向联动（§4.4）：给当前终端标签发一条 cd。只发给活着的、且不已在
      // 该目录的；shell 回报 OSC 7 后 follow 判定同路径，不会回环。
      const st = useWorkspace.getState();
      const tab = st.tabs.find((t) => t.id === st.activeId);
      if (tab?.status === "live" && tab.cwd !== path) {
        staleGuard.current = { tabId: tab.id, prev: cur };
        termSockets.get(tab.id)?.send(buildCdBytes(path, tab.shell));
      }
    },
    [setCwd],
  );

  // 正向联动：活动终端报出新 cwd，文件区跟过去（§4.4「cd 到哪文件就到哪」）。
  const activeTabCwd = useWorkspace((st) => st.tabs.find((t) => t.id === st.activeId)?.cwd);
  const activeTabId = useWorkspace((st) => st.activeId);
  useEffect(() => {
    if (activeTabCwd === undefined) return;
    const g = staleGuard.current;
    if (g.tabId === activeTabId) {
      if (activeTabCwd === g.prev) return; // cd 之前就在路上的旧报告，见上
      staleGuard.current = { tabId: null, prev: null }; // 任何新值都清挡板
    }
    if (activeTabCwd !== cwd) setCwd(activeTabCwd);
  }, [activeTabCwd, activeTabId, cwd, setCwd]);
  const jump = useCallback(
    (target: string, pushTo: React.MutableRefObject<string[]>) => {
      const st = useWorkspace.getState();
      if (st.cwd !== null) pushTo.current.push(st.cwd);
      setCwd(target);
      const tab = st.tabs.find((t) => t.id === st.activeId);
      if (tab?.status === "live" && tab.cwd !== target) {
        staleGuard.current = { tabId: tab.id, prev: st.cwd };
        termSockets.get(tab.id)?.send(buildCdBytes(target, tab.shell));
      }
    },
    [setCwd],
  );
  const goBack = useCallback(() => {
    const prev = past.current.pop();
    if (prev !== undefined) jump(prev, future);
  }, [jump]);
  const goForward = useCallback(() => {
    const next = future.current.pop();
    if (next !== undefined) jump(next, past);
  }, [jump]);

  return (
    <div className={s.workspace}>
      <div className={cx(railCollapsed && s.railCollapsed)}>
        {!railCollapsed && <QuickAccess current={cwd} onGo={navigate} />}
      </div>
      <div className={s.main}>
        <FileList
          leading={
            <Button
              iconOnly
              size="sm"
              aria-label={railCollapsed ? "展开快速访问" : "折叠快速访问"}
              onClick={toggleRail}
            >
              <PanelLeft size={14} />
            </Button>
          }
          path={cwd}
          platform={platform}
          onNavigate={navigate}
          canBack={past.current.length > 0}
          canForward={future.current.length > 0}
          onBack={goBack}
          onForward={goForward}
        />
        <TerminalPanel />
      </div>
    </div>
  );
}
