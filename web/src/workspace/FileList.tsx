import {
  ArrowLeft,
  ArrowRight,
  ArrowUp,
  Eye,
  EyeOff,
  File,
  Folder,
  Link2,
  RefreshCw,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  Button,
  type Column,
  EmptyState,
  ErrorState,
  ProgressLine,
  Segmented,
  Table,
  TableSkeleton,
  Toolbar,
  ToolbarSpacer,
} from "@/components";
import { fmtBytes } from "@/lib/fmt";
import { fileIconUrl, folderIconUrl } from "./icons";
import { joinPath, type Platform, parentPath } from "./path";
import { useWorkspace } from "./store";
import { TileGrid } from "./TileGrid";
import { type DirEntry, type FileSortKey, useDirListing } from "./useDirListing";
import s from "./Workspace.module.css";

/** 虚拟滚动的行高兜底值；真实值在首行渲染后量出。 */
const ROW_FALLBACK = 28;
/** 视口外多渲染几行，滚动时不露白。 */
const OVERSCAN = 12;

/** `0o644` → `rw-r--r--`。Windows 上是合成值（`providers/fs/windows.rs`），照样能读。 */
export function fmtMode(mode: number): string {
  const bits = "rwx";
  let out = "";
  for (let i = 8; i >= 0; i--) {
    out += mode & (1 << i) ? bits[(8 - i) % 3] : "-";
  }
  return out;
}

function fmtMtime(ts: number): string {
  const d = new Date(ts * 1000);
  const sameYear = d.getFullYear() === new Date().getFullYear();
  const pad = (n: number) => String(n).padStart(2, "0");
  const md = `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  return sameYear ? md : `${d.getFullYear()}-${md}`;
}

function KindIcon({ entry }: { entry: DirEntry }) {
  if (entry.kind === "symlink") return <Link2 size={14} strokeWidth={1.5} aria-label="符号链接" />;
  // 内置图标集（§4.7）：文件夹总有图标，文件认不出时回落到通用形状。
  const src = entry.kind === "dir" ? folderIconUrl(entry.name) : fileIconUrl(entry.name);
  if (src)
    return (
      <img
        src={src}
        width={16}
        height={16}
        alt={entry.kind === "dir" ? "目录" : ""}
        aria-hidden={entry.kind !== "dir"}
      />
    );
  if (entry.kind === "dir")
    return <Folder size={14} strokeWidth={1.5} className={s.kindDir} aria-label="目录" />;
  return <File size={14} strokeWidth={1.5} className={s.kindFile} aria-hidden="true" />;
}

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
 * 文件区（§4.5）：列表视图（虚拟滚动 + 服务端分页排序）与平铺图标视图。
 *
 * 目录项导航一律走 [`joinPath`]——Windows 驱动器根的 `name` 就是完整路径,
 * `joinPath` 知道这件事，组件里不手写拼接。
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
  const dirSort = useWorkspace((st) => st.dirSort);
  const setDirSort = useWorkspace((st) => st.setDirSort);

  const {
    entries,
    total,
    skipped,
    error,
    isPending,
    isFetching,
    hasNextPage,
    isFetchingNextPage,
    fetchNextPage,
    refresh,
  } = useDirListing(path, dirSort);

  const [selected, setSelected] = useState<string | null>(null);
  /** 地址栏编辑中的值；`null` = 未在编辑，跟随 `path` 显示。 */
  const [editing, setEditing] = useState<string | null>(null);
  const scrollRef = useRef<HTMLDivElement | null>(null);

  // ---- 虚拟滚动：只渲染视口附近的行（§4.5「不能一次渲染出来」）----
  const [scrollTop, setScrollTop] = useState(0);
  const [viewportH, setViewportH] = useState(600);
  const [rowH, setRowH] = useState(ROW_FALLBACK);

  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setViewportH(el.clientHeight));
    ro.observe(el);
    setViewportH(el.clientHeight);
    return () => ro.disconnect();
  }, []);

  // 首行真实高度量一次：行高猜错时滚动条与内容错位。
  const measureRow = useCallback((el: HTMLDivElement | null) => {
    const tr = el?.querySelector("tbody tr:not([data-spacer])");
    const h = tr?.getBoundingClientRect().height;
    if (h && h > 8) setRowH(h);
  }, []);

  // 换目录回到顶部并清选中；刷新（同 path 重取）不走这里，滚动与选中原地保留。
  // biome-ignore lint/correctness/useExhaustiveDependencies: 刻意只对 path 变化生效
  useEffect(() => {
    scrollRef.current?.scrollTo({ top: 0 });
    setScrollTop(0);
    setSelected(null);
    setEditing(null);
  }, [path]);

  const up = path === null ? null : parentPath(path, platform);

  // 隐藏文件按 dotfile 约定过滤（在已加载的页内做；后端分页不知道这条约定）。
  const visible = hideHidden ? entries.filter((e) => !e.name.startsWith(".")) : entries;
  const hiddenCount = hideHidden ? entries.length - visible.length : 0;

  const start = Math.max(0, Math.floor(scrollTop / rowH) - OVERSCAN);
  const end = Math.min(visible.length, Math.ceil((scrollTop + viewportH) / rowH) + OVERSCAN);
  const slice = visible.slice(start, end);

  // 滚近已加载末尾就取下一页。
  useEffect(() => {
    if (hasNextPage && !isFetchingNextPage && end >= visible.length - OVERSCAN) {
      void fetchNextPage();
    }
  }, [end, visible.length, hasNextPage, isFetchingNextPage, fetchNextPage]);

  const enter = useCallback(
    (entry: DirEntry) => {
      if (path === null || entry.kind !== "dir") return;
      onNavigate(joinPath(path, entry.name, platform));
    },
    [path, platform, onNavigate],
  );

  /** 点表头：同键翻方向，异键切键（升序起步）。排序在服务端，翻页也一致。 */
  const onHeaderClick = (key: string) => {
    if (key !== "name" && key !== "size" && key !== "mtime") return;
    const k = key as FileSortKey;
    setDirSort(dirSort.key === k ? { key: k, desc: !dirSort.desc } : { key: k, desc: false });
  };

  const columns: readonly Column<DirEntry>[] = [
    {
      key: "name",
      header: "名称",
      mono: true,
      render: (e) => (
        <span className={s.nameCell}>
          <KindIcon entry={e} />
          <span>{e.name}</span>
          {e.target && <span className={s.linkTarget}>→ {e.target}</span>}
        </span>
      ),
    },
    {
      key: "size",
      header: "大小",
      numeric: true,
      width: "6.5em",
      render: (e) => (e.kind === "dir" ? "—" : fmtBytes(e.size_bytes)),
    },
    {
      key: "mtime",
      header: "修改",
      numeric: true,
      width: "9em",
      render: (e) => fmtMtime(e.mtime_ts),
    },
    {
      key: "mode",
      header: "权限",
      mono: true,
      width: "7em",
      render: (e) => fmtMode(e.mode),
    },
    {
      key: "owner",
      header: "属主",
      mono: true,
      dim: true,
      width: "7em",
      render: (e) => e.user ?? String(e.uid),
    },
  ];

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
        <input
          className={s.pathBar}
          aria-label="路径，回车跳转"
          title={path ?? ""}
          value={editing ?? path ?? ""}
          onChange={(e) => setEditing(e.target.value)}
          onFocus={(e) => e.target.select()}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              const target = (editing ?? "").trim();
              if (target && target !== path) onNavigate(target);
              setEditing(null);
              e.currentTarget.blur();
            }
            if (e.key === "Escape") {
              setEditing(null);
              e.currentTarget.blur();
            }
          }}
          onBlur={() => setEditing(null)}
          spellCheck={false}
        />
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
      {isFetching && !isPending && <ProgressLine />}
      <div
        ref={scrollRef}
        className={s.fileScroll}
        onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}
      >
        {path === null || (isPending && entries.length === 0) ? (
          <TableSkeleton />
        ) : error ? (
          <ErrorState
            title="读不到这个目录。"
            detail={(error as { message?: string }).message ?? String(error)}
            onRetry={refresh}
          />
        ) : viewMode === "tiles" ? (
          <TileGrid
            entries={visible}
            pathOf={(e) => (path === null ? e.name : joinPath(path, e.name, platform))}
            selected={selected}
            onSelect={setSelected}
            onEnterDir={enter}
          />
        ) : (
          <div ref={measureRow}>
            <Table
              caption={`目录 ${path} 的内容`}
              columns={columns}
              rows={slice}
              rowKey={(e) => e.name}
              selectedKey={selected}
              onSelect={(e) => {
                setSelected(e.name);
                enter(e);
              }}
              onHeaderClick={onHeaderClick}
              sortedBy={{ key: dirSort.key, desc: dirSort.desc }}
              empty={<EmptyState title="这个目录是空的" />}
              leadingRow={
                start > 0 && <tr data-spacer aria-hidden style={{ height: start * rowH }} />
              }
              trailingRow={
                visible.length - end > 0 && (
                  <tr data-spacer aria-hidden style={{ height: (visible.length - end) * rowH }} />
                )
              }
            />
          </div>
        )}
        {isFetchingNextPage && <p className={s.skippedNote}>正在加载更多……</p>}
        {total > entries.length && !hasNextPage && null}
        {hiddenCount > 0 && <p className={s.skippedNote}>{hiddenCount} 个隐藏条目未显示</p>}
        {skipped > 0 && <p className={s.skippedNote}>{skipped} 个条目因无权限或已消失被跳过</p>}
        {total > 0 && (
          <p className={s.skippedNote}>
            共 {total} 项{entries.length < total ? `，已加载 ${entries.length}` : ""}
          </p>
        )}
      </div>
    </div>
  );
}
