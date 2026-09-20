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

/** 一次层叠切换：`from` 是让位的那层，方向决定谁在上面怎么动。 */
interface Layering {
  from: string;
  dir: "push" | "pop";
}

/**
 * 文件区（§4.5）：工具栏（导航/地址栏补全/视图切换）+ 层叠的目录层。
 *
 * **层叠导航**（负责人 2026-09-20 要求，macOS 手感）：进入子目录时新层从右
 * 推入、旧层向左略退压暗；返回祖先时顶层滑出还原。跨目录跳转（快速访问、
 * 地址栏）不属于层级关系，直接切换不演。同屏最多两层，动画完只留当前层。
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

  // ---- 层叠状态机 ----
  const [layer, setLayer] = useState<Layering | null>(null);
  const prevPath = useRef<string | null>(path);
  // biome-ignore lint/correctness/useExhaustiveDependencies: 只在 path 变化时判定一次方向
  useEffect(() => {
    const old = prevPath.current;
    prevPath.current = path;
    if (old === null || path === null || old === path) {
      setLayer(null);
      return;
    }
    // 减少动态：不止关动画，第二层根本不建（也让端到端测试可确定）。
    if (globalThis.matchMedia?.("(prefers-reduced-motion: reduce)").matches) {
      setLayer(null);
      return;
    }
    if (isDescendant(path, old, platform)) setLayer({ from: old, dir: "push" });
    else if (isDescendant(old, path, platform)) setLayer({ from: old, dir: "pop" });
    else setLayer(null);
  }, [path]);
  useEffect(() => {
    if (!layer) return;
    const t = setTimeout(() => setLayer(null), 320);
    return () => clearTimeout(t);
  }, [layer]);

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

  const up = path === null ? null : parentPath(path, platform);

  // 层叠渲染：垫底层在前、动的那层在后（DOM 顺序即层序）。key 稳定，
  // 动画结束后当前层原地保留，不重挂、不闪。
  const panes: React.ReactNode[] = [];
  if (layer?.dir === "push") {
    panes.push(
      <ListPane
        key={`p:${layer.from}`}
        path={layer.from}
        platform={platform}
        onNavigate={onNavigate}
        className={s.paneUnderPush}
      />,
      <ListPane
        key={`p:${path}`}
        path={path}
        platform={platform}
        onNavigate={onNavigate}
        className={s.panePushIn}
      />,
    );
  } else if (layer?.dir === "pop") {
    panes.push(
      <ListPane
        key={`p:${path}`}
        path={path}
        platform={platform}
        onNavigate={onNavigate}
        className={s.paneUnderPop}
      />,
      <ListPane
        key={`p:${layer.from}`}
        path={layer.from}
        platform={platform}
        onNavigate={onNavigate}
        className={s.panePopOut}
      />,
    );
  } else {
    panes.push(
      <ListPane key={`p:${path}`} path={path} platform={platform} onNavigate={onNavigate} />,
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
