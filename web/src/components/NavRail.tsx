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

export interface NavSection {
  label: string;
  items: readonly NavItem[];
}

export interface NavRailProps {
  /** 平铺条目。与 `sections` 二选一，都传时 `sections` 优先。 */
  items?: readonly NavItem[];
  /** 分组条目（观测 / 操作 / 系统）。窄条模式下组标题隐藏、只剩发丝线。 */
  sections?: readonly NavSection[];
  current: string;
  onNavigate: (id: string) => void;
  /** < 900px 时折叠成 44px 图标条（spec §8） */
  narrow?: boolean;
  /** 作为外层自定义栏的一段嵌入使用：去掉自身宽度 / 底色 / 右边线 */
  bare?: boolean;
  footer?: ReactNode;
}

export function NavRail({
  items,
  sections,
  current,
  onNavigate,
  narrow,
  bare,
  footer,
}: NavRailProps) {
  const row = (it: NavItem) => {
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
  };

  return (
    <nav className={cx(s.rail, narrow && s.narrow, bare && s.bare)} aria-label="主导航">
      {sections
        ? sections.map((sec, i) => (
            <div key={sec.label}>
              {i > 0 && <div className={s.groupSep} aria-hidden="true" />}
              <div className={s.groupLabel}>{sec.label}</div>
              {sec.items.map(row)}
            </div>
          ))
        : (items ?? []).map(row)}
      {footer}
    </nav>
  );
}
