import { useCallback, useEffect, useRef, useState } from "react";
import { StatusDot } from "@/components";
import { cx } from "@/lib/cx";
import s from "./Svc.module.css";
import { SvcIcon } from "./SvcIcon";
import type { Unit } from "./useUnits";

export interface UnitSort {
  key: "name" | "type" | "state" | "enable";
  order: "asc" | "desc";
}

/** 行高,与 tokens.css 的 `--row` 一致(虚拟化按它算窗口)。 */
const ROW_H = 30;
const OVERSCAN = 12;
const N_COLS = 4;

/** 活动状态 → 排序权重(failed 永远浮最上)与状态标记。 */
const STATE_RANK: Record<string, number> = {
  failed: 0,
  activating: 1,
  deactivating: 1,
  reloading: 1,
  active: 2,
  inactive: 3,
  unknown: 4,
};

function dotOf(u: Unit): "run" | "stop" | "fail" | "unknown" {
  switch (u.active_state) {
    case "active":
      return "run";
    case "failed":
      return "fail";
    case "inactive":
      return "stop";
    default:
      return "unknown";
  }
}

/** 启用态 → 短文案。三值以上,不做 checkbox(types 文档的告诫)。 */
export function enableLabel(u: Unit): { text: string; tone?: "dim" | "warn" } {
  switch (u.enable_state) {
    case "enabled":
      return { text: "自启" };
    case "enabled_runtime":
      return { text: "自启（本次）" };
    case "disabled":
      return { text: "手动", tone: "dim" };
    case "masked":
      return { text: "已屏蔽", tone: "warn" };
    // 只在本次启动内有效,与持久化的 masked 不是一回事:重启后自行消失。
    case "masked_runtime":
      return { text: "已屏蔽（本次）", tone: "warn" };
    case "static":
      return { text: "static", tone: "dim" };
    case "indirect":
      return { text: "indirect", tone: "dim" };
    case undefined:
    case null:
      return { text: "—", tone: "dim" };
    default:
      return { text: u.enable_state, tone: "dim" };
  }
}

export function unitCmp(sort: UnitSort): (a: Unit, b: Unit) => number {
  const dir = sort.order === "asc" ? 1 : -1;
  const rank = (u: Unit) => STATE_RANK[u.active_state] ?? 4;
  switch (sort.key) {
    case "name":
      return (a, b) => a.name.localeCompare(b.name) * dir;
    case "type":
      return (a, b) =>
        (a.unit_type.localeCompare(b.unit_type) || a.name.localeCompare(b.name)) * dir;
    case "enable":
      return (a, b) =>
        ((a.enable_state ?? "~").localeCompare(b.enable_state ?? "~") ||
          a.name.localeCompare(b.name)) * dir;
    case "state":
      return (a, b) => (rank(a) - rank(b) || a.name.localeCompare(b.name)) * dir;
  }
}

export function UnitTable({
  rows,
  sort,
  onSort,
  selected,
  onSelect,
}: {
  rows: readonly Unit[];
  sort: UnitSort;
  onSort: (next: UnitSort) => void;
  selected: string | null;
  onSelect: (name: string) => void;
}) {
  const areaRef = useRef<HTMLDivElement | null>(null);
  const [win, setWin] = useState({ from: 0, to: 80 });
  const measure = useCallback(() => {
    const el = areaRef.current;
    if (!el) return;
    const from = Math.max(0, Math.floor(el.scrollTop / ROW_H) - OVERSCAN);
    const to = Math.ceil((el.scrollTop + el.clientHeight) / ROW_H) + OVERSCAN;
    setWin((cur) => (cur.from === from && cur.to === to ? cur : { from, to }));
  }, []);
  useEffect(() => {
    measure();
    const el = areaRef.current;
    if (!el) return;
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, [measure]);
  // biome-ignore lint/correctness/useExhaustiveDependencies: 行数变化时重测窗口
  useEffect(measure, [rows.length, measure]);

  const from = Math.min(win.from, rows.length);
  const to = Math.min(win.to, rows.length);

  const th = (key: UnitSort["key"], label: string) => {
    const active = sort.key === key;
    return (
      <th
        className={cx(active && s.thOn)}
        aria-sort={active ? (sort.order === "asc" ? "ascending" : "descending") : undefined}
      >
        <button
          type="button"
          className={s.thBtn}
          onClick={() =>
            onSort(
              active
                ? { key, order: sort.order === "asc" ? "desc" : "asc" }
                : { key, order: "asc" },
            )
          }
        >
          {label}
          <span className={s.thArrow}>{active ? (sort.order === "desc" ? "▾" : "▴") : ""}</span>
        </button>
      </th>
    );
  };

  return (
    <div className={s.tableArea} ref={areaRef} onScroll={measure}>
      <table className={s.tbl}>
        <thead>
          <tr>
            {th("name", "名称")}
            {th("type", "类型")}
            {th("state", "状态")}
            {th("enable", "启用")}
          </tr>
        </thead>
        <tbody>
          {from > 0 && (
            <tr>
              <td colSpan={N_COLS} style={{ height: from * ROW_H, padding: 0 }} />
            </tr>
          )}
          {rows.slice(from, to).map((u, i) => {
            const en = enableLabel(u);
            return (
              <tr
                key={u.name}
                className={cx(s.row, (from + i) % 2 === 1 && s.alt, selected === u.name && s.rowOn)}
                onClick={() => onSelect(u.name)}
              >
                <td className={s.name}>
                  <span className={s.nameWrap}>
                    <SvcIcon name={u.name} />
                    <b>{u.name}</b>
                    {u.description !== u.name && <span className={s.desc}>{u.description}</span>}
                  </span>
                </td>
                <td className={s.dim}>{u.unit_type}</td>
                <td>
                  <StatusDot state={dotOf(u)} label={u.sub_state} />
                </td>
                <td className={cx(en.tone === "dim" && s.dim, en.tone === "warn" && s.warn)}>
                  {en.text}
                </td>
              </tr>
            );
          })}
          {to < rows.length && (
            <tr>
              <td colSpan={N_COLS} style={{ height: (rows.length - to) * ROW_H, padding: 0 }} />
            </tr>
          )}
        </tbody>
      </table>
    </div>
  );
}
