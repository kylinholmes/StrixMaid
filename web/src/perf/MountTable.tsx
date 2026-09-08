import { useState } from "react";
import { useNavigate } from "react-router-dom";
import type { components } from "@/api/schema";
import { cx } from "@/lib/cx";
import { fmtBytes, fmtPct } from "@/lib/fmt";
import { latestOf, type Ring, seriesKey } from "@/metrics/live";
import s from "./Perf.module.css";

type FilesystemInfo = components["schemas"]["FilesystemInfo"];

/** 路径中间截断（spec §5.2:尾部截断会切掉最有信息量的文件名）。 */
function midTrunc(p: string, max = 44): string {
  if (p.length <= max) return p;
  const head = Math.ceil((max - 1) * 0.55);
  const tail = max - 1 - head;
  return `${p.slice(0, head)}…${p.slice(-tail)}`;
}

/** 服务端直接给 used_bytes(= total − free,含 root 保留块),不要用 total − available 自算。 */
function usedOf(f: FilesystemInfo): number {
  return f.used_bytes;
}

/**
 * 挂载点合计。读写按**去重后的后端设备**求和(08 §6.3——同盘三个挂载点相加会把
 * 同一份 IO 数三遍)。容量同理:同组内 total 全部相等视为共享容器——APFS 的
 * statfs 对容器内每个卷报的 used/total 都是**容器级**的同一份数,used 和 total
 * 都只取一次(相加会数 N 遍,曾把 284 GiB 加成 1.4 TiB);total 不等才是
 * 真分区(Linux),按分区各自相加。
 */
export function mountTotals(list: readonly FilesystemInfo[], rings: ReadonlyMap<string, Ring>) {
  const byDev = new Map<string, FilesystemInfo[]>();
  const solo: FilesystemInfo[] = [];
  for (const f of list) {
    if (!f.backing_dev) {
      solo.push(f);
      continue;
    }
    const g = byDev.get(f.backing_dev);
    if (g) g.push(f);
    else byDev.set(f.backing_dev, [f]);
  }
  let used = 0;
  let total = 0;
  for (const group of byDev.values()) {
    const first = group[0];
    if (!first) continue;
    const shared = group.every((f) => f.total_bytes === first.total_bytes);
    if (shared) {
      used += Math.max(...group.map(usedOf));
      total += first.total_bytes;
    } else {
      used += group.reduce((a, f) => a + usedOf(f), 0);
      total += group.reduce((a, f) => a + f.total_bytes, 0);
    }
  }
  for (const f of solo) {
    used += usedOf(f);
    total += f.total_bytes;
  }
  // 后端设备可能没有 IO 序列(macOS 的 APFS 容器 disk1/disk3,IO 计在物理盘 disk0 上)
  // ——一个都测不到时是「—」,不是 0
  let rd: number | null = null;
  let wr: number | null = null;
  for (const dev of byDev.keys()) {
    const r = latestOf(rings, seriesKey("disk.read_bytes", `dev=${dev}`));
    const w = latestOf(rings, seriesKey("disk.write_bytes", `dev=${dev}`));
    if (r !== null) rd = (rd ?? 0) + r;
    if (w !== null) wr = (wr ?? 0) + w;
  }
  return { used, total, devCount: byDev.size, rd, wr };
}

function Bar({ pct }: { pct: number }) {
  const cls = pct >= 0.8 ? s.mbarCrit : pct >= 0.7 ? s.mbarWarn : undefined;
  return (
    <span className={s.mbar}>
      <i className={cls} style={{ width: `${Math.min(100, pct * 100).toFixed(1)}%` }} />
    </span>
  );
}

/**
 * 挂载点表（08 §6.3）:默认折叠成一行合计,展开后每行整行可点、跳到承载它的盘。
 * 出现两处:磁盘组页(全部挂载点)与磁盘详情页(只有这块盘的)。
 */
export function MountTable({
  filesystems,
  rings,
  liveDevs,
  caption,
  defaultOpen = false,
}: {
  filesystems: readonly FilesystemInfo[];
  rings: ReadonlyMap<string, Ring>;
  /** 有实时序列的块设备——只有这些行可点进详情页 */
  liveDevs: readonly string[];
  caption: string;
  defaultOpen?: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  const navigate = useNavigate();

  if (filesystems.length === 0) return null;

  // 有块设备的行排前(按设备聚拢),伪文件系统沉底
  const rows = [...filesystems].sort((a, b) => {
    if (!!a.backing_dev !== !!b.backing_dev) return a.backing_dev ? -1 : 1;
    return (
      (a.backing_dev ?? "").localeCompare(b.backing_dev ?? "") ||
      a.mount_point.localeCompare(b.mount_point)
    );
  });
  const sum = mountTotals(filesystems, rings);
  const sumPct = sum.total > 0 ? sum.used / sum.total : 0;

  const toggle = () => setOpen((v) => !v);

  return (
    <div className={s.mtblWrap}>
      <table className={s.mtbl}>
        <thead>
          <tr>
            <th>{caption}</th>
            <th>后端设备</th>
            <th>使用量</th>
            <th className={s.mr}>已用 / 容量</th>
            <th className={s.mr}>读取</th>
            <th className={s.mr}>写入</th>
          </tr>
        </thead>
        <tbody>
          {/* biome-ignore lint/a11y/useSemanticElements: 表格行没法换成 <button>;spec §10 对可点行的规定就是 role="button"+tabIndex+键盘 */}
          <tr
            className={cx(s.mrow, s.mtot)}
            role="button"
            tabIndex={0}
            aria-expanded={open}
            onClick={toggle}
            onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                toggle();
              }
            }}
          >
            <td>
              <span className={s.mtw} aria-hidden="true">
                {open ? "▾" : "▸"}
              </span>
              合计
            </td>
            <td className={s.msub}>
              {filesystems.length} 个 · {sum.devCount || "无"} 块设备
            </td>
            <td>
              <Bar pct={sumPct} />
            </td>
            <td className={cx(s.mr, s.mnum)}>
              {fmtPct(sumPct)}
              <span className={s.msub}>
                {" "}
                {fmtBytes(sum.used)} / {fmtBytes(sum.total)}
              </span>
            </td>
            <td className={cx(s.mr, s.mnum)}>{sum.rd !== null ? `${fmtBytes(sum.rd)}/s` : "—"}</td>
            <td className={cx(s.mr, s.mnum)}>{sum.wr !== null ? `${fmtBytes(sum.wr)}/s` : "—"}</td>
          </tr>

          {open &&
            rows.map((f) => {
              const used = usedOf(f);
              const pct = f.total_bytes > 0 ? used / f.total_bytes : 0;
              const dev = f.backing_dev ?? null;
              const clickable = dev !== null && liveDevs.includes(dev);
              const rd = dev ? latestOf(rings, seriesKey("disk.read_bytes", `dev=${dev}`)) : null;
              const wr = dev ? latestOf(rings, seriesKey("disk.write_bytes", `dev=${dev}`)) : null;
              const go = () => {
                if (clickable && dev) navigate(`/performance/disk/${encodeURIComponent(dev)}`);
              };
              return (
                <tr
                  key={f.mount_point}
                  className={cx(s.mrow, clickable && s.mlink)}
                  role={clickable ? "button" : undefined}
                  tabIndex={clickable ? 0 : undefined}
                  aria-label={clickable ? `打开 ${dev}` : undefined}
                  onClick={go}
                  onKeyDown={
                    clickable
                      ? (e) => {
                          if (e.key === "Enter" || e.key === " ") {
                            e.preventDefault();
                            go();
                          }
                        }
                      : undefined
                  }
                >
                  <td className={s.mpath} title={f.mount_point}>
                    {midTrunc(f.mount_point)}
                  </td>
                  <td>
                    {dev ? (
                      <span className={s.mdev}>{dev}</span>
                    ) : (
                      <span className={s.msub}>无块设备</span>
                    )}
                    <span className={s.msub}> · {f.fs_type}</span>
                  </td>
                  <td>
                    <Bar pct={pct} />
                  </td>
                  <td className={cx(s.mr, s.mnum)}>
                    {fmtPct(pct)}
                    <span className={s.msub}>
                      {" "}
                      {fmtBytes(used)} / {fmtBytes(f.total_bytes)}
                    </span>
                  </td>
                  <td className={cx(s.mr, s.mnum)}>{rd !== null ? `${fmtBytes(rd)}/s` : "—"}</td>
                  <td className={cx(s.mr, s.mnum)}>{wr !== null ? `${fmtBytes(wr)}/s` : "—"}</td>
                </tr>
              );
            })}
        </tbody>
      </table>
    </div>
  );
}
