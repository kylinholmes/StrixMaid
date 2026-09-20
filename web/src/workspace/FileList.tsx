import { useIsFetching, useQueryClient } from "@tanstack/react-query";
import { ArrowLeft, ArrowRight, ArrowUp, Eye, EyeOff, RefreshCw } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api } from "@/api/client";
import { Button, ProgressLine, Segmented, Toolbar, ToolbarSpacer } from "@/components";
import { cx } from "@/lib/cx";
import { ListPane } from "./ListPane";
import { isDescendant, joinPath, type Platform, parentPath, splitForCompletion } from "./path";
import { useWorkspace } from "./store";
import s from "./Workspace.module.css";

export interface FileListProps {
  path: string | null;
  platform: Platform;
  /** 所有导航（进目录、上一级）都汇到这里；历史由上层管理。 */
  onNavigate: (path: string) => void;
  canBack: boolean;
  canForward: boolean;
  onBack: () => void;
  onForward: () => void;
  /** 工具栏最左侧的附加内容（工作区放快速访问栏的开关）。 */
  leading?: React.ReactNode;
}

/**
 * 文件区（§4.5）：工具栏（导航/地址栏补全/视图切换）+ 常驻两层的目录层叠。
 *
 * **层叠导航**（负责人 2026-09-20 定，macOS 手感）：**父目录始终垫在下面**，
 * 顶层向右让出一条边让父层透出来，点那条边即返回。垫层不用维护栈——它永远
 * 就是 `parentPath(当前)`，推导即可。进子目录时旧顶层**原地降为垫层**（同
 * key，位置与压暗走 CSS 过渡）、新层从右推入；回上级时顶层滑出、垫层升顶。
 * 跨层级跳转（快速访问、地址栏）不演动画，直接换层。
 */
export function FileList({
  path,
  platform,
  onNavigate,
  canBack,
  canForward,
  onBack,
  onForward,
  leading,
}: FileListProps) {
  const hideHidden = useWorkspace((st) => st.hideHidden);
  const toggleHidden = useWorkspace((st) => st.toggleHidden);
  const viewMode = useWorkspace((st) => st.viewMode);
  const setViewMode = useWorkspace((st) => st.setViewMode);

  const qc = useQueryClient();
  const fetching = useIsFetching({ queryKey: ["dir", path] }) > 0;
  const refresh = () => {
    if (path !== null) void qc.invalidateQueries({ queryKey: ["dir", path] });
  };

  /** 地址栏编辑中的值；`null` = 未在编辑，跟随 `path` 显示。 */
  const [editing, setEditing] = useState<string | null>(null);

  // ---- 层叠状态：方向只决定「演不演」，垫层本身由 parentPath 推导 ----
  /** 刚发生 push：给新顶层挂一次「从右推入」的入场动画。 */
  const [pushAnim, setPushAnim] = useState(false);
  /** 刚发生 pop：被弹掉的那层短暂留在最上面演「滑出」。 */
  const [leaving, setLeaving] = useState<string | null>(null);
  const prevPath = useRef<string | null>(path);
  // biome-ignore lint/correctness/useExhaustiveDependencies: 只在 path 变化时判定一次方向
  useEffect(() => {
    const old = prevPath.current;
    prevPath.current = path;
    if (old === null || path === null || old === path) return;
    // 减少动态：布局照旧两层，动画全免。
    if (globalThis.matchMedia?.("(prefers-reduced-motion: reduce)").matches) return;
    if (isDescendant(path, old, platform)) {
      setPushAnim(true);
      setLeaving(null);
    } else if (isDescendant(old, path, platform)) {
      setLeaving(old);
      setPushAnim(false);
    } else {
      setPushAnim(false);
      setLeaving(null);
    }
  }, [path]);
  useEffect(() => {
    if (!pushAnim && leaving === null) return;
    const t = setTimeout(() => {
      setPushAnim(false);
      setLeaving(null);
    }, 420);
    return () => clearTimeout(t);
  }, [pushAnim, leaving]);

  // ---- 地址栏补全：父目录的子目录按前缀过滤 ----
  const [sugs, setSugs] = useState<string[]>([]);
  const [sugIdx, setSugIdx] = useState(-1);

  useEffect(() => {
    if (editing === null) {
      setSugs([]);
      return;
    }
    const split = splitForCompletion(editing.trim(), platform);
    if (!split) {
      setSugs([]);
      return;
    }
    // 去抖 + 短缓存：同一父目录连续敲字不重复打后端。
    const t = setTimeout(async () => {
      try {
        const data = await qc.fetchQuery({
          queryKey: ["complete", split.parent],
          staleTime: 10_000,
          queryFn: async () => {
            const { data: d, error: e } = await api.GET("/api/v1/files", {
              params: {
                query: { path: split.parent, limit: 200, sort: "name", order: "asc" },
              },
            });
            if (e) throw e;
            return d;
          },
        });
        const pref = split.prefix.toLowerCase();
        setSugs(
          (data.entries ?? [])
            .filter((en) => en.kind === "dir" && en.name.toLowerCase().startsWith(pref))
            .slice(0, 8)
            .map((en) => joinPath(split.parent, en.name, platform)),
        );
        setSugIdx(-1);
      } catch {
        setSugs([]); // 父目录读不到就不补，不打扰输入
      }
    }, 150);
    return () => clearTimeout(t);
  }, [editing, platform, qc]);

  const acceptSuggestion = (target: string) => {
    onNavigate(target);
    setEditing(null);
    setSugs([]);
  };

  useEffect(() => {
    setEditing(null);
  }, []);

  const parent = path === null ? null : parentPath(path, platform);
  const up = parent;

  // 常驻两层：垫层永远是父目录（key 稳定——push 时旧顶层同 key 原地降级，
  // 位置/压暗由 CSS 过渡接管）；再叠一条可点的「返回」边；pop 的旧顶层
  // 以 leaving 短暂盖在最上面演滑出。DOM 顺序即层序。
  const panes: React.ReactNode[] = [];
  // 当前目录在父层里叫什么（垫层里高亮它，一眼看出「我从哪进来的」）。
  const currentName =
    path !== null && parent !== null
      ? path.slice(parent.length).replace(/^[\\/]/, "") || path
      : null;
  if (parent !== null) {
    panes.push(
      <ListPane
        key={`p:${parent}`}
        path={parent}
        platform={platform}
        onNavigate={onNavigate}
        pane="under"
        className={s.paneUnder}
        markName={currentName}
      />,
    );
  }
  panes.push(
    <ListPane
      key={`p:${path}`}
      path={path}
      platform={platform}
      onNavigate={onNavigate}
      pane="top"
      className={cx(parent !== null && s.paneTop, pushAnim && s.panePushIn)}
    />,
  );
  if (leaving !== null) {
    panes.push(
      <ListPane
        key={`p:${leaving}`}
        path={leaving}
        platform={platform}
        onNavigate={onNavigate}
        pane="leaving"
        className={cx(s.paneTop, s.panePopOut)}
      />,
    );
  }

  return (
    <div className={s.fileArea}>
      <Toolbar>
        {leading}
        <Button iconOnly size="sm" aria-label="后退" disabled={!canBack} onClick={onBack}>
          <ArrowLeft size={14} />
        </Button>
        <Button iconOnly size="sm" aria-label="前进" disabled={!canForward} onClick={onForward}>
          <ArrowRight size={14} />
        </Button>
        <Button
          iconOnly
          size="sm"
          aria-label="上一级"
          disabled={up === null}
          onClick={() => up !== null && onNavigate(up)}
        >
          <ArrowUp size={14} />
        </Button>
        <span className={s.pathWrap}>
          <input
            className={s.pathBar}
            role="combobox"
            aria-label="路径，回车跳转"
            aria-expanded={sugs.length > 0}
            aria-controls="path-sugs"
            title={path ?? ""}
            value={editing ?? path ?? ""}
            onChange={(e) => setEditing(e.target.value)}
            onFocus={(e) => e.target.select()}
            onKeyDown={(e) => {
              if (e.key === "ArrowDown" && sugs.length > 0) {
                e.preventDefault();
                setSugIdx((i) => (i + 1) % sugs.length);
                return;
              }
              if (e.key === "ArrowUp" && sugs.length > 0) {
                e.preventDefault();
                setSugIdx((i) => (i <= 0 ? sugs.length - 1 : i - 1));
                return;
              }
              if (e.key === "Tab" && sugIdx >= 0 && sugs[sugIdx]) {
                // Tab 只补进输入框，继续往下敲；Enter 才跳转。
                e.preventDefault();
                setEditing(sugs[sugIdx]);
                return;
              }
              if (e.key === "Enter") {
                const chosen = sugIdx >= 0 ? sugs[sugIdx] : null;
                const target = (chosen ?? editing ?? "").trim();
                if (target && target !== path) onNavigate(target);
                setEditing(null);
                setSugs([]);
                e.currentTarget.blur();
              }
              if (e.key === "Escape") {
                if (sugs.length > 0) {
                  setSugs([]); // 第一次 Esc 收下拉，第二次才还原输入
                  return;
                }
                setEditing(null);
                e.currentTarget.blur();
              }
            }}
            onBlur={() => {
              setEditing(null);
              setSugs([]);
            }}
            spellCheck={false}
          />
          {editing !== null && sugs.length > 0 && (
            <div id="path-sugs" className={s.pathSugs} role="listbox" aria-label="路径补全">
              {sugs.map((p2, i) => (
                <button
                  key={p2}
                  type="button"
                  role="option"
                  aria-selected={i === sugIdx}
                  className={cx(s.pathSug, i === sugIdx && s.pathSugActive)}
                  // mousedown 抢在 input 失焦之前，click 就来不及了
                  onMouseDown={(ev) => {
                    ev.preventDefault();
                    acceptSuggestion(p2);
                  }}
                >
                  {p2}
                </button>
              ))}
            </div>
          )}
        </span>
        <ToolbarSpacer />
        <Segmented
          label="文件视图"
          options={[
            { value: "list", label: "列表" },
            { value: "tiles", label: "平铺" },
          ]}
          value={viewMode}
          onChange={setViewMode}
        />
        <Button
          iconOnly
          size="sm"
          aria-label={hideHidden ? "显示隐藏文件" : "隐藏隐藏文件"}
          title={hideHidden ? "显示以 . 开头的条目" : "隐藏以 . 开头的条目"}
          onClick={toggleHidden}
        >
          {hideHidden ? <EyeOff size={14} /> : <Eye size={14} />}
        </Button>
        <Button iconOnly size="sm" aria-label="刷新" onClick={refresh}>
          <RefreshCw size={14} />
        </Button>
      </Toolbar>
      {fetching && <ProgressLine />}
      <div className={s.scrollWrap}>{panes}</div>
    </div>
  );
}
