import type { CSSProperties } from "react";
import { cx } from "@/lib/cx";
import s from "./Overlay.module.css";

export interface MenuItem {
  id: string;
  label: string;
  disabled?: boolean;
  destructive?: boolean;
}

export interface MenuProps {
  items: readonly MenuItem[];
  onPick: (id: string) => void;
  label: string;
  style?: CSSProperties;
}

/** 下拉菜单。不加遮罩（只有对话框加），靠双层阴影与边框离地。 */
export function Menu({ items, onPick, label, style }: MenuProps) {
  return (
    <ul className={s.menu} style={style} aria-label={label}>
      {items.map((it) => (
        <li key={it.id}>
          <button
            type="button"
            className={cx(s.item, it.destructive && s.itemDanger)}
            disabled={it.disabled}
            onClick={() => onPick(it.id)}
          >
            {it.label}
          </button>
        </li>
      ))}
    </ul>
  );
}
