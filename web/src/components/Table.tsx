import type { ReactNode } from "react";
import { cx } from "@/lib/cx";
import s from "./Table.module.css";

export interface Column<T> {
  key: string;
  header: string;
  /** 右对齐 + 等宽 + tabular-nums */
  numeric?: boolean;
  /** 机器给的字符串（服务名、路径、设备名） */
  mono?: boolean;
  dim?: boolean;
  width?: string;
  render: (row: T) => ReactNode;
}

export interface TableProps<T> {
  columns: readonly Column<T>[];
  rows: readonly T[];
  rowKey: (row: T) => string;
  selectedKey?: string | null;
  onSelect?: (row: T) => void;
  caption: string;
  /** 行数为 0 时渲染的内容 */
  empty?: ReactNode;
  /** 表头可点（服务端排序等）。给了它，表头渲染成按钮并回调列 key。 */
  onHeaderClick?: (key: string) => void;
  /** 当前排序状态，用于表头的方向指示。仅在 `onHeaderClick` 存在时有意义。 */
  sortedBy?: { key: string; desc: boolean };
  /** 表体前的额外行（虚拟滚动的上撑高行等），原样塞进 tbody 顶部。 */
  leadingRow?: ReactNode;
  /** 表体后的额外行（虚拟滚动的下撑高行、加载中行等）。 */
  trailingRow?: ReactNode;
}

export function Table<T>({
  columns,
  rows,
  rowKey,
  selectedKey,
  onSelect,
  caption,
  empty,
  onHeaderClick,
  sortedBy,
  leadingRow,
  trailingRow,
}: TableProps<T>) {
  return (
    <table className={s.table}>
      <caption className="sr-only" style={{ position: "absolute", left: -9999 }}>
        {caption}
      </caption>
      <thead>
        <tr>
          {columns.map((c) => (
            <th
              key={c.key}
              className={c.numeric ? s.num : undefined}
              style={{ width: c.width }}
              aria-sort={
                sortedBy?.key === c.key ? (sortedBy.desc ? "descending" : "ascending") : undefined
              }
            >
              {onHeaderClick ? (
                <button type="button" className={s.sortable} onClick={() => onHeaderClick(c.key)}>
                  {c.header}
                  {sortedBy?.key === c.key && (
                    <span aria-hidden>{sortedBy.desc ? " ↓" : " ↑"}</span>
                  )}
                </button>
              ) : (
                c.header
              )}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {leadingRow}
        {rows.length === 0 && empty ? (
          <tr className={s.empty}>
            <td colSpan={columns.length}>{empty}</td>
          </tr>
        ) : (
          rows.map((row) => {
            const key = rowKey(row);
            const selected = selectedKey != null && key === selectedKey;
            return (
              <tr
                key={key}
                className={cx(s.row, onSelect && s.clickable)}
                aria-selected={onSelect ? selected : undefined}
                tabIndex={onSelect ? 0 : undefined}
                onClick={onSelect ? () => onSelect(row) : undefined}
                onKeyDown={
                  onSelect
                    ? (e: React.KeyboardEvent<HTMLTableRowElement>) => {
                        if (e.key === "Enter" || e.key === " ") {
                          e.preventDefault();
                          onSelect(row);
                        }
                      }
                    : undefined
                }
              >
                {columns.map((c) => (
                  <td
                    key={c.key}
                    className={cx(c.numeric && s.num, c.mono && s.mono, c.dim && s.dim)}
                  >
                    {c.render(row)}
                  </td>
                ))}
              </tr>
            );
          })
        )}
        {trailingRow}
      </tbody>
    </table>
  );
}
