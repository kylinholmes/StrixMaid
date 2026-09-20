import { File, Folder, Link2 } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { type Column, EmptyState, ErrorState, Table, TableSkeleton } from "@/components";
import { cx } from "@/lib/cx";
import { fmtBytes } from "@/lib/fmt";
import { fileIconUrl, folderIconUrl } from "./icons";
import { OverlayScrollbar } from "./OverlayScrollbar";
import { joinPath, type Platform } from "./path";
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

export interface ListPaneProps {
  path: string | null;
  platform: Platform;
  onNavigate: (path: string) => void;
  /** 层叠导航的动画类（推入/滑出/垫底），由 FileList 编排。 */
  className?: string;
  /** 层角色标记（top/under/leaving），测试与调试按它定位。 */
  pane?: string;
}

/**
 * 一「层」目录（列表或平铺）：取数 + 虚拟滚动 + 自绘滚动条自成一体。
 * 层叠导航（进目录像 macOS 那样推入新层）要求同屏能渲染两层，
 * 所以这些状态必须住在层内而不是 FileList 里。
 */
export function ListPane({ path, platform, onNavigate, className, pane }: ListPaneProps) {
  const hideHidden = useWorkspace((st) => st.hideHidden);
  const viewMode = useWorkspace((st) => st.viewMode);
  const dirSort = useWorkspace((st) => st.dirSort);
  const setDirSort = useWorkspace((st) => st.setDirSort);

  const {
    entries,
    total,
    skipped,
    error,
    isPending,
    hasNextPage,
    isFetchingNextPage,
    fetchNextPage,
    refresh,
  } = useDirListing(path, dirSort);

  const [selected, setSelected] = useState<string | null>(null);
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
    <div className={cx(s.pane, className)} data-pane={pane}>
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
        {hiddenCount > 0 && <p className={s.skippedNote}>{hiddenCount} 个隐藏条目未显示</p>}
        {skipped > 0 && <p className={s.skippedNote}>{skipped} 个条目因无权限或已消失被跳过</p>}
        {total > 0 && (
          <p className={s.skippedNote}>
            共 {total} 项{entries.length < total ? `，已加载 ${entries.length}` : ""}
          </p>
        )}
      </div>
      <OverlayScrollbar target={scrollRef} />
    </div>
  );
}
