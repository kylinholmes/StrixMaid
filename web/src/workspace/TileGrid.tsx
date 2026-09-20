import { File, Folder, Link2 } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { cx } from "@/lib/cx";
import { fileIconUrl, folderIconUrl } from "./icons";
import { useSysIcon } from "./sysicons";
import { fetchThumb, thumbEligible } from "./thumbs";
import type { DirEntry } from "./useDirListing";
import s from "./Workspace.module.css";

/** 进入视口才取缩略图：一个几百张图的目录不该在打开瞬间全量拉取。 */
function useLazyThumb(path: string, eligible: boolean) {
  const ref = useRef<HTMLDivElement | null>(null);
  const [url, setUrl] = useState<string | null>(null);

  useEffect(() => {
    if (!eligible) return;
    const el = ref.current;
    if (!el) return;
    let live = true;
    const io = new IntersectionObserver((io_entries) => {
      if (!io_entries.some((e) => e.isIntersecting)) return;
      io.disconnect();
      void fetchThumb(path).then((u) => {
        if (live && u) setUrl(u);
      });
    });
    io.observe(el);
    return () => {
      live = false;
      io.disconnect();
    };
  }, [path, eligible]);

  return { ref, url };
}

function Tile({
  entry,
  path,
  selected,
  onOpen,
  onSelect,
}: {
  entry: DirEntry;
  path: string;
  selected: boolean;
  onOpen: () => void;
  onSelect: () => void;
}) {
  const { ref, url } = useLazyThumb(path, thumbEligible(entry));
  // 优先级：缩略图 > 系统真图标 > 内置图标集 > 通用形状（与 KindIcon 一致）。
  const sys = useSysIcon(entry.kind === "file" ? entry.name : null);
  const builtin =
    entry.kind === "dir"
      ? folderIconUrl(entry.name)
      : entry.kind === "file"
        ? (sys ?? fileIconUrl(entry.name))
        : null;
  const icon = builtin ? (
    <img src={builtin} width={68} height={68} alt="" />
  ) : entry.kind === "dir" ? (
    <Folder size={44} strokeWidth={1.2} className={s.kindDir} />
  ) : entry.kind === "symlink" ? (
    <Link2 size={44} strokeWidth={1.2} />
  ) : (
    <File size={44} strokeWidth={1.2} className={s.kindFile} />
  );

  return (
    <button
      type="button"
      className={cx(s.tile, selected && s.tileSelected)}
      title={entry.name}
      onClick={() => {
        onSelect();
        onOpen();
      }}
    >
      <div ref={ref} className={s.tileIcon}>
        {url ? <img src={url} alt="" loading="lazy" /> : icon}
      </div>
      <span className={s.tileName}>{entry.name}</span>
    </button>
  );
}

export interface TileGridProps {
  entries: readonly DirEntry[];
  /** 条目名 → 完整路径（平台感知的拼接在上层做好）。 */
  pathOf: (e: DirEntry) => string;
  selected: string | null;
  onSelect: (name: string) => void;
  /** 打开一个目录（文件暂无动作）。 */
  onEnterDir: (e: DirEntry) => void;
}

/** 平铺图标视图（§4.5 的第二种视图）。图片出缩略图（§4.7），其余用类型图标。 */
export function TileGrid({ entries, pathOf, selected, onSelect, onEnterDir }: TileGridProps) {
  return (
    <div className={s.tileGrid}>
      {entries.map((e) => (
        <Tile
          key={e.name}
          entry={e}
          path={pathOf(e)}
          selected={selected === e.name}
          onSelect={() => onSelect(e.name)}
          onOpen={() => {
            if (e.kind === "dir") onEnterDir(e);
          }}
        />
      ))}
    </div>
  );
}
