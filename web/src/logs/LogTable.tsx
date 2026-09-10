import { useCallback, useEffect, useRef, useState } from "react";
import { cx } from "@/lib/cx";
import s from "./Logs.module.css";
import type { LogEntry } from "./useLogs";

/** 行高,与 tokens.css 的 `--row` 一致。 */
const ROW_H = 30;
const OVERSCAN = 15;
const N_COLS = 4;
/** 距底部多少像素内触发翻旧页 */
const LOAD_MORE_PX = 600;

interface PrioStyle {
  label: string;
  tone?: "warn" | "crit" | "dim";
}

/** 级别 → 短标签。色彩预算:err+ 用 crit、warning 用 warn,其余不占色(spec §4)。 */
const PRIO: Record<string, PrioStyle> = {
  emerg: { label: "EMERG", tone: "crit" },
  alert: { label: "ALERT", tone: "crit" },
  crit: { label: "CRIT", tone: "crit" },
  err: { label: "ERR", tone: "crit" },
  warning: { label: "WARN", tone: "warn" },
  notice: { label: "NOTICE" },
  info: { label: "INFO", tone: "dim" },
  debug: { label: "DEBUG", tone: "dim" },
};

function fmtTime(ts: number, us: number): string {
  const d = new Date(ts * 1000);
  const two = (n: number) => String(n).padStart(2, "0");
  const ms = String(Math.floor(us / 1000)).padStart(3, "0");
  return `${two(d.getHours())}:${two(d.getMinutes())}:${two(d.getSeconds())}.${ms}`;
}

export function srcOf(e: LogEntry): string {
  return e.unit ?? e.identifier ?? (e.transport === "kernel" ? "kernel" : "—");
}

export function LogTable({
  rows,
  selected,
  onSelect,
  onPickUnit,
  onAtTop,
  onLoadOlder,
  loadingOlder,
  hasMore,
}: {
  rows: readonly LogEntry[];
  selected: string | null;
  onSelect: (cursor: string) => void;
  /** 点来源列 = 下钻到该 unit 的日志(设置 unit 过滤) */
  onPickUnit: (unit: string) => void;
  onAtTop: (v: boolean) => void;
  onLoadOlder: () => void;
  loadingOlder: boolean;
  hasMore: boolean;
}) {
  const areaRef = useRef<HTMLDivElement | null>(null);
  const [win, setWin] = useState({ from: 0, to: 80 });
  const measure = useCallback(() => {
    const el = areaRef.current;
    if (!el) return;
    onAtTop(el.scrollTop < ROW_H);
    if (el.scrollHeight - el.scrollTop - el.clientHeight < LOAD_MORE_PX) onLoadOlder();
    const from = Math.max(0, Math.floor(el.scrollTop / ROW_H) - OVERSCAN);
    const to = Math.ceil((el.scrollTop + el.clientHeight) / ROW_H) + OVERSCAN;
    setWin((cur) => (cur.from === from && cur.to === to ? cur : { from, to }));
  }, [onAtTop, onLoadOlder]);
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

  return (
    <div className={s.tableArea} ref={areaRef} onScroll={measure}>
      <table className={s.tbl}>
        <thead>
          <tr>
            <th>时间</th>
            <th>级别</th>
            <th>来源</th>
            <th>消息</th>
          </tr>
        </thead>
        <tbody>
          {from > 0 && (
            <tr>
              <td colSpan={N_COLS} style={{ height: from * ROW_H, padding: 0 }} />
            </tr>
          )}
          {rows.slice(from, to).map((e, i) => {
            const p = PRIO[e.priority] ?? { label: e.priority, tone: "dim" as const };
            return (
              <tr
                key={e.cursor}
                className={cx(
                  s.row,
                  (from + i) % 2 === 1 && s.alt,
                  selected === e.cursor && s.rowOn,
                )}
                onClick={() => onSelect(e.cursor)}
              >
                <td
                  className={cx(s.mono, s.time)}
                  title={new Date(e.ts * 1000).toLocaleString("zh-CN", { hour12: false })}
                >
                  {fmtTime(e.ts, e.us)}
                </td>
                <td>
                  <span
                    className={cx(
                      s.prio,
                      p.tone === "warn" && s.prioWarn,
                      p.tone === "crit" && s.prioCrit,
                      p.tone === "dim" && s.prioDim,
                    )}
                  >
                    {p.label}
                  </span>
                </td>
                <td className={cx(s.mono, s.src)}>
                  {e.unit ? (
                    <button
                      type="button"
                      className={s.srcBtn}
                      title={`只看 ${e.unit} 的日志`}
                      onClick={(ev) => {
                        ev.stopPropagation();
                        onPickUnit(e.unit ?? "");
                      }}
                    >
                      {srcOf(e)}
                    </button>
                  ) : (
                    <span title={srcOf(e)}>{srcOf(e)}</span>
                  )}
                </td>
                <td className={cx(s.mono, s.msg)} title={e.message}>
                  {e.message}
                </td>
              </tr>
            );
          })}
          {to < rows.length && (
            <tr>
              <td colSpan={N_COLS} style={{ height: (rows.length - to) * ROW_H, padding: 0 }} />
            </tr>
          )}
          <tr>
            <td colSpan={N_COLS} className={s.foot}>
              {loadingOlder ? "加载更旧的日志…" : hasMore ? "" : rows.length > 0 ? "已到最早" : ""}
            </td>
          </tr>
        </tbody>
      </table>
    </div>
  );
}
