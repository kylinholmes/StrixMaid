import { useQuery } from "@tanstack/react-query";
import { PanelLeft } from "lucide-react";
import { useCallback, useEffect, useRef } from "react";
import { capabilitiesQuery } from "@/app/queries";
import { Button } from "@/components";
import { cx } from "@/lib/cx";
import { useSession } from "@/session/useSession";
import { FileList } from "./FileList";
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

  // 入口决定面板初始态；只在进入时定一次，之后由用户操作接管。
  const applied = useRef(false);
  useEffect(() => {
    if (applied.current) return;
    applied.current = true;
    setPanelCollapsed(initial === "files");
  }, [initial, setPanelCollapsed]);

  // 起点：主目录（capabilities 与会话就绪后才推得出）。
  useEffect(() => {
    if (cwd === null && user && caps.data) {
      setCwd(guessHome(osId, user.username, user.uid));
    }
  }, [cwd, user, caps.data, osId, setCwd]);

  // 导航历史：所有改 cwd 的路都过 navigate，后退/前进才知道来龙去脉。
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
  const goBack = useCallback(() => {
    const prev = past.current.pop();
    if (prev === undefined) return;
    const cur = useWorkspace.getState().cwd;
    if (cur !== null) future.current.push(cur);
    setCwd(prev);
  }, [setCwd]);
  const goForward = useCallback(() => {
    const next = future.current.pop();
    if (next === undefined) return;
    const cur = useWorkspace.getState().cwd;
    if (cur !== null) past.current.push(cur);
    setCwd(next);
  }, [setCwd]);

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
