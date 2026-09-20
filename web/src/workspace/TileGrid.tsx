import { File, Folder, Link2 } from "lucide-react";
import { cx } from "@/lib/cx";
import { fileIconUrl, folderIconUrl } from "./icons";
import type { Platform } from "./path";
import { entrySubject, useEntryIcon } from "./sysicons";
import { orientationTransform, thumbEligible, useLazyThumb } from "./thumbs";
import type { DirEntry } from "./useDirListing";
import s from "./Workspace.module.css";

function Tile({
  entry,
  path,
  platform,
  selected,
  onOpen,
  onSelect,
}: {
  entry: DirEntry;
  path: string;
  platform: Platform;
  selected: boolean;
  onOpen: () => void;
  onSelect: () => void;
}) {
  const { ref, thumb } = useLazyThumb<HTMLDivElement>(path, thumbEligible(entry));
  // 优先级：缩略图 > 系统真图标（按路径/按类型，判定在 iconKeysOf 里编译期
  // 穷尽）> 内置集 > 通用形状，与 ListPane 的 KindIcon 完全一致。
  const sys = useEntryIcon(entrySubject(entry, path), platform);
  const src =
    sys ??
    (entry.kind === "dir"
      ? folderIconUrl(entry.name)
      : entry.kind === "file"
        ? fileIconUrl(entry.name)
        : null);
  const icon = src ? (
    <img src={src} width={68} height={68} alt="" />
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
        {thumb ? (
          <img
            src={thumb.url}
            alt=""
            loading="lazy"
            style={{ transform: orientationTransform(thumb.orientation) }}
          />
        ) : (
          icon
        )}
      </div>
      <span className={s.tileName}>{entry.name}</span>
    </button>
  );
}

export interface TileGridProps {
  entries: readonly DirEntry[];
  /** 条目名 → 完整路径（平台感知的拼接在上层做好）。 */
  pathOf: (e: DirEntry) => string;
  platform: Platform;
  selected: string | null;
  onSelect: (name: string) => void;
  /** 打开一个目录（文件暂无动作）。 */
  onEnterDir: (e: DirEntry) => void;
}

/** 平铺图标视图（§4.5 的第二种视图）。图片出缩略图（§4.7），其余用类型图标。 */
export function TileGrid({
  entries,
  pathOf,
  platform,
  selected,
  onSelect,
  onEnterDir,
}: TileGridProps) {
  return (
    <div className={s.tileGrid}>
      {entries.map((e) => (
        <Tile
          key={e.name}
          entry={e}
          path={pathOf(e)}
          platform={platform}
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
