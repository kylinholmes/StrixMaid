import { useCallback, useEffect, useRef, useState } from "react";
import { cssVar, withAlpha } from "@/components/Plot";
import { cx } from "@/lib/cx";
import { fmtBytes, fmtPct } from "@/lib/fmt";
import s from "./Proc.module.css";
import type { Proc, SortState, TreeRow } from "./tree";

interface StateStyle {
  ch: string;
  label: string;
  tone?: "warn" | "dim";
}

const UNKNOWN_STATE: StateStyle = { ch: "?", label: "未知", tone: "dim" };

/** 状态 → htop 式单字母。全称进 title,异常态才上警示色(spec §4 状态色预算)。 */
const STATE: Record<string, StateStyle> = {
  running: { ch: "R", label: "运行" },
  sleeping: { ch: "S", label: "睡眠", tone: "dim" },
  disk_sleep: { ch: "D", label: "不可中断睡眠(等 IO)", tone: "warn" },
  zombie: { ch: "Z", label: "僵尸", tone: "warn" },
  stopped: { ch: "T", label: "已停止", tone: "warn" },
  tracing_stop: { ch: "t", label: "被追踪暂停", tone: "warn" },
  dead: { ch: "X", label: "已死亡", tone: "warn" },
  idle: { ch: "I", label: "空闲(内核线程)", tone: "dim" },
  unknown: { ch: "?", label: "未知", tone: "dim" },
};

/** 内存热力的满格点:占整机 20% 的进程就算「很大」,再大不加深。 */
const MEM_HEAT_FULL = 20;
/** 热力底色的透明度上限(单色相顺序色阶,Win10 任务管理器同款思路)。 */
const HEAT_MAX = 0.32;

/** 行高,必须与 tokens.css 的 `--row` 一致(虚拟化按它算窗口)。 */
const ROW_H = 30;
/** 可视区外上下各多渲染几行,快速滚动不露白。 */
const OVERSCAN = 12;
/** 列数,spacer 行的 colSpan。 */
const N_COLS = 10;

function heat(hex: string, frac: number): React.CSSProperties | undefined {
  if (!(frac > 0)) return undefined;
  return { background: withAlpha(hex, Math.min(1, frac) * HEAT_MAX) };
}

function ioOf(p: Proc): number | null {
  if (p.io_read_rate === undefined && p.io_write_rate === undefined) return null;
  return (p.io_read_rate ?? 0) + (p.io_write_rate ?? 0);
}

/** 启动时刻:当天只给时分,更早的带月-日。完整日期进 title。 */
function fmtStart(ts: number): string {
  const d = new Date(ts * 1000);
  const two = (n: number) => String(n).padStart(2, "0");
  const hm = `${two(d.getHours())}:${two(d.getMinutes())}`;
  const now = new Date();
  return d.toDateString() === now.toDateString()
    ? hm
    : `${two(d.getMonth() + 1)}-${two(d.getDate())} ${hm}`;
}

/** 点击未排序列时的初始方向:文本与 pid 升序,数值降序(先看最占资源的)。 */
const ASC_FIRST = new Set<SortState["key"]>(["name", "user", "pid", "nice"]);

export function ProcTable({
  rows,
  sort,
  onSort,
  selected,
  onSelect,
  onToggle,
  cpuTotal,
  memPct,
}: {
  rows: readonly TreeRow[];
  sort: SortState;
  onSort: (next: SortState) => void;
  selected: number | null;
  onSelect: (pid: number) => void;
  onToggle: (pid: number) => void;
  /** 表头总量(任务管理器同款):整机 CPU% 与内存% */
  cpuTotal: number | null;
  memPct: number | null;
}) {
  const cpuHue = cssVar("--cpu");
  const memHue = cssVar("--mem");
  const diskHue = cssVar("--disk");
  // IO 没有天然的 100%,热力按当前帧内最大值归一(htop 的做法)
  const ioMax = rows.reduce((m, r) => Math.max(m, ioOf(r.p) ?? 0), 0);

  // ---- 虚拟化:只渲染可视窗口 ± OVERSCAN,其余用 spacer 行占高 ----
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
  // 行数变少(折叠/筛选)时浏览器会钳 scrollTop,布局后重算一次窗口
  // biome-ignore lint/correctness/useExhaustiveDependencies: 只需在行数变化时重测
  useEffect(measure, [rows.length, measure]);

  const from = Math.min(win.from, rows.length);
  const to = Math.min(win.to, rows.length);
  const slice = rows.slice(from, to);

  const th = (
    key: SortState["key"] | null,
    label: string,
    opts?: { num?: boolean; total?: string },
  ) => {
    const active = key !== null && sort.key === key;
    const flip = () =>
      key !== null &&
      onSort(
        active
          ? { key, order: sort.order === "desc" ? "asc" : "desc" }
          : { key, order: ASC_FIRST.has(key) ? "asc" : "desc" },
      );
    return (
      <th
        className={cx(opts?.num && s.num, active && s.thOn)}
        aria-sort={active ? (sort.order === "asc" ? "ascending" : "descending") : undefined}
      >
        {key !== null ? (
          <button type="button" className={s.thBtn} onClick={flip}>
            {label}
            {opts?.total !== undefined && <span className={s.thTotal}>{opts.total}</span>}
            <span className={s.thArrow}>{active ? (sort.order === "desc" ? "▾" : "▴") : ""}</span>
          </button>
        ) : (
          <span className={s.thBtn}>
            {label}
            {opts?.total !== undefined && <span className={s.thTotal}>{opts.total}</span>}
          </span>
        )}
      </th>
    );
  };

  return (
    <div className={s.tableArea} ref={areaRef} onScroll={measure}>
      <table className={s.tbl}>
        <caption className="sr-only" style={{ position: "absolute", left: -9999 }}>
          进程树
        </caption>
        <thead>
          <tr>
            {th("name", "名称")}
            {th("pid", "PID", { num: true })}
            {th("user", "用户")}
            {th(null, "状态")}
            {th("nice", "nice", { num: true })}
            {th("cpu", "CPU", {
              num: true,
              total: cpuTotal !== null ? fmtPct(cpuTotal / 100) : undefined,
            })}
            {th("mem", "内存", {
              num: true,
              total: memPct !== null ? fmtPct(memPct / 100) : undefined,
            })}
            {th("io", "磁盘 IO", { num: true })}
            {th("threads", "线程", { num: true })}
            {th("start", "启动", { num: true })}
          </tr>
        </thead>
        <tbody>
          {from > 0 && (
            <tr>
              <td colSpan={N_COLS} style={{ height: from * ROW_H, padding: 0 }} />
            </tr>
          )}
          {slice.map(({ p, depth, children, expanded }, i) => {
            const st = STATE[p.state] ?? UNKNOWN_STATE;
            const io = ioOf(p);
            return (
              <tr
                key={p.pid}
                className={cx(s.row, (from + i) % 2 === 1 && s.alt, selected === p.pid && s.rowOn)}
                onClick={() => onSelect(p.pid)}
              >
                <td className={s.name}>
                  <span style={{ paddingLeft: depth * 16 }} className={s.nameWrap}>
                    {children > 0 ? (
                      <button
                        type="button"
                        className={s.twist}
                        aria-label={expanded ? "折叠子进程" : "展开子进程"}
                        onClick={(e) => {
                          e.stopPropagation();
                          onToggle(p.pid);
                        }}
                      >
                        {expanded ? "▾" : "▸"}
                      </button>
                    ) : (
                      <span className={s.twist} />
                    )}
                    <b>{p.name}</b>
                    {p.cmdline && p.cmdline !== p.name && (
                      <span className={s.cmd}>{p.cmdline}</span>
                    )}
                  </span>
                </td>
                <td className={cx(s.num, s.mono)}>{p.pid}</td>
                <td className={s.user}>{p.user ?? p.uid}</td>
                <td>
                  <span
                    className={cx(
                      s.state,
                      st.tone === "warn" && s.stateWarn,
                      st.tone === "dim" && s.stateDim,
                    )}
                    title={st.label}
                  >
                    {st.ch}
                  </span>
                </td>
                <td className={cx(s.num, s.mono)}>{p.nice}</td>
                <td
                  className={cx(s.num, s.mono)}
                  style={heat(cpuHue, p.cpu_percent / 100)}
                  title={`CPU ${p.cpu_percent.toFixed(1)}%（单核跑满 = 100%）`}
                >
                  {p.cpu_percent.toFixed(1)}%
                </td>
                <td
                  className={cx(s.num, s.mono)}
                  style={heat(memHue, p.mem_percent / MEM_HEAT_FULL)}
                  title={`占物理内存 ${p.mem_percent.toFixed(1)}%`}
                >
                  {fmtBytes(p.rss_bytes)}
                </td>
                <td
                  className={cx(s.num, s.mono)}
                  style={io !== null && ioMax > 0 ? heat(diskHue, io / ioMax) : undefined}
                  title={
                    io === null
                      ? "测不到:无权限或平台不提供 per-process IO"
                      : `读 ${fmtBytes(p.io_read_rate ?? 0)}/s · 写 ${fmtBytes(p.io_write_rate ?? 0)}/s`
                  }
                >
                  {io === null ? "—" : `${fmtBytes(io)}/s`}
                </td>
                <td className={cx(s.num, s.mono)}>{p.threads}</td>
                <td
                  className={cx(s.num, s.mono)}
                  title={new Date(p.start_ts * 1000).toLocaleString("zh-CN", { hour12: false })}
                >
                  {fmtStart(p.start_ts)}
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
