import { ArrowLeft, ArrowRight, ArrowUp, File, Folder, Link2, RefreshCw } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { components } from "@/api/schema";
import {
  Button,
  type Column,
  EmptyState,
  ErrorState,
  ProgressLine,
  Table,
  TableSkeleton,
  Toolbar,
  ToolbarSpacer,
} from "@/components";
import { fmtBytes } from "@/lib/fmt";
import { joinPath, type Platform, parentPath } from "./path";
import { useDirListing } from "./useDirListing";
import s from "./Workspace.module.css";

type DirEntry = components["schemas"]["DirEntryInfo"];

/** 默认渲染的行数上限；普通目录远小于它，只有 WinSxS 这类目录会碰到。 */
const RENDER_CAP = 500;

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

function KindIcon({ kind }: { kind: DirEntry["kind"] }) {
  if (kind === "dir")
    return <Folder size={14} strokeWidth={1.5} className={s.kindDir} aria-label="目录" />;
  if (kind === "symlink") return <Link2 size={14} strokeWidth={1.5} aria-label="符号链接" />;
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
 * 文件只读列表（§4.5 的列表视图）。平铺图标、缩略图、分页都在 D 期。
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
  const { data, error, isPending, isFetching, refresh } = useDirListing(path);
  const [selected, setSelected] = useState<string | null>(null);
  const [showAll, setShowAll] = useState(false);
  const scrollRef = useRef<HTMLDivElement | null>(null);

  // 换目录回到顶部并清选中；刷新（同 path 重取）不走这里，滚动与选中原地保留。
  // biome-ignore lint/correctness/useExhaustiveDependencies: 刻意只对 path 变化生效
  useEffect(() => {
    scrollRef.current?.scrollTo({ top: 0 });
    setSelected(null);
    setShowAll(false);
  }, [path]);

  const up = path === null ? null : parentPath(path, platform);

  // 大目录兜底（D 期的分页 + 虚拟滚动到位前）：一次渲染上万行会把页面卡死，
  // 默认只渲染前 RENDER_CAP 行，其余点「显示全部」明确换取。
  const allRows = data?.entries ?? [];
  const capped = !showAll && allRows.length > RENDER_CAP;
  const visibleRows = capped ? allRows.slice(0, RENDER_CAP) : allRows;

  const enter = (entry: DirEntry) => {
    if (path === null) return;
    if (entry.kind !== "dir") return;
    onNavigate(joinPath(path, entry.name, platform));
  };

  const columns: readonly Column<DirEntry>[] = [
    {
      key: "name",
      header: "名称",
      mono: true,
      render: (e) => (
        <span className={s.nameCell}>
          <KindIcon kind={e.kind} />
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
        <span className={s.pathBar} title={path ?? ""}>
          {path ?? ""}
        </span>
        <ToolbarSpacer />
        <Button iconOnly size="sm" aria-label="刷新" onClick={refresh}>
          <RefreshCw size={14} />
        </Button>
      </Toolbar>
      {isFetching && !isPending && <ProgressLine />}
      <div ref={scrollRef} className={s.fileScroll}>
        {path === null || (isPending && !data) ? (
          <TableSkeleton />
        ) : error ? (
          <ErrorState
            title="读不到这个目录。"
            detail={(error as { message?: string }).message ?? String(error)}
            onRetry={refresh}
          />
        ) : (
          <>
            <Table
              caption={`目录 ${path} 的内容`}
              columns={columns}
              rows={visibleRows}
              rowKey={(e) => e.name}
              selectedKey={selected}
              onSelect={(e) => {
                setSelected(e.name);
                enter(e);
              }}
              empty={<EmptyState title="这个目录是空的" />}
            />
            {capped && (
              <div className={s.showAllRow}>
                <Button size="sm" onClick={() => setShowAll(true)}>
                  共 {allRows.length} 项，已显示前 {RENDER_CAP} 项——显示全部
                </Button>
              </div>
            )}
          </>
        )}
        {data && (data.skipped ?? 0) > 0 && (
          <p className={s.skippedNote}>{data.skipped} 个条目因无权限或已消失被跳过</p>
        )}
      </div>
    </div>
  );
}
