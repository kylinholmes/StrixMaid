import type { LucideIcon } from "lucide-react";
import type { ReactNode } from "react";
import { cx } from "@/lib/cx";
import s from "./NavRail.module.css";

export interface NavItem {
  id: string;
  label: string;
  icon: LucideIcon;
  count?: string;
  countTone?: "default" | "bad";
  /** 有这个能力但当前用户无权 —— 显示但禁用（spec §6）。
      能力**不存在**时不要传 disabled，而是根本不要把这一项放进列表。 */
  disabled?: boolean;
  disabledHint?: string;
}

export interface NavRailProps {
  items: readonly NavItem[];
  current: string;
  onNavigate: (id: string) => void;
  /** < 900px 时折叠成 44px 图标条（spec §8） */
  narrow?: boolean;
  footer?: ReactNode;
}

export function NavRail({ items, current, onNavigate, narrow, footer }: NavRailProps) {
  return (
    <nav className={cx(s.rail, narrow && s.narrow)} aria-label="主导航">
      {items.map((it) => {
        const Icon = it.icon;
        return (
          <button
            key={it.id}
            type="button"
            className={cx(s.row, it.disabled && s.disabled)}
            aria-current={it.id === current ? "page" : undefined}
            aria-disabled={it.disabled || undefined}
            /* 图标条模式下只剩图标，title 是唯一的名字来源 */
            title={narrow ? it.label : it.disabledHint}
            onClick={it.disabled ? undefined : () => onNavigate(it.id)}
          >
            <Icon size={15} aria-hidden="true" />
            <span className={s.label}>{it.label}</span>
            <span className={s.spacer} />
            {it.count && !it.disabled && (
              <span className={cx(s.count, it.countTone === "bad" && s.countBad)}>{it.count}</span>
            )}
            {it.disabled && <span className={s.lock}>需管理访问</span>}
          </button>
        );
      })}
      {footer}
    </nav>
  );
}
