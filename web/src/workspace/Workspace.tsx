import { useQuery } from "@tanstack/react-query";
import { PanelLeft } from "lucide-react";
import { useCallback, useEffect, useRef } from "react";
import { useLocation } from "react-router-dom";
import { capabilitiesQuery } from "@/app/queries";
import { Button } from "@/components";
import { cx } from "@/lib/cx";
import { useSession } from "@/session/useSession";
import { FileList } from "./FileList";
import { sideButtonAction } from "./mousenav";
import { guessHome, platformOf } from "./path";
import { QuickAccess } from "./QuickAccess";
import { useWorkspace } from "./store";
import { TerminalPanel } from "./TerminalPanel";
import s from "./Workspace.module.css";

export interface WorkspaceProps {
  /** 从哪个入口进来：`terminal` → 终端面板展开；`files` → 收起（§4.1）。 */
  initial: "terminal" | "files";
}

/**
 * 工作区（`docs/roadmap/12-workspace.md`）：终端与文件合成一块。
 * 终端与文件同处一页；**不做目录同步**（曾按 §4.4 实现过双向联动，
 * 负责人 2026-09-20 裁定移除：注入 cd 与跟随跳转带来体感卡顿，不值）。
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
  const navigate = useCallback(
    (path: string) => {
      const cur = useWorkspace.getState().cwd;
      if (cur === path) return;
      if (cur !== null) past.current.push(cur);
      future.current = [];
      setCwd(path);
    },
    [setCwd],
  );

  const jump = useCallback(
    (target: string, pushTo: React.MutableRefObject<string[]>) => {
      const st = useWorkspace.getState();
      if (st.cwd !== null) pushTo.current.push(st.cwd);
      setCwd(target);
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

  // 鼠标侧键 → 文件区的前进 / 后退。
  //
  // 与 ⌃` 同样挂在 window 的 capture 阶段：终端聚焦时也要收得到。
  // `preventDefault` 是承重的，理由见 `mousenav.ts`——不拦的话浏览器会
  // 自己把 SPA 退到上一个路由。
  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      const action = sideButtonAction(e.button);
      if (action === null) return;
      e.preventDefault();
      if (action === "back") goBack();
      else goForward();
    };
    window.addEventListener("mousedown", onDown, true);
    return () => window.removeEventListener("mousedown", onDown, true);
  }, [goBack, goForward]);

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
