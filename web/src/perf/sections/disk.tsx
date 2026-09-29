import { fmtBytes, fmtPct } from "@/lib/fmt";
import { useDiscovery } from "@/metrics/discovery";
import { latestOf, latestSum, liveMembers, seriesKey, useLive } from "@/metrics/live";
import { MountTable } from "../MountTable";
import s from "../Perf.module.css";
import { DISK_RESOURCE, MemberGrid } from "./members";
import {
  AggChart,
  ChartFrame,
  type NumItem,
  type PerfView,
  PsiChart,
  type SectionProps,
  StatFact,
  useSystemInfo,
} from "./shared";

/* ================= 磁盘 ================= */

export function DiskSection({ range, view, onLayer }: SectionProps & { view: PerfView }) {
  const rings = useLive((st) => st.rings);
  const discovery = useDiscovery();
  const info = useSystemInfo();
  const devs = liveMembers(
    rings,
    "disk.util",
    "dev",
    discovery.data?.members("disk.util", "dev") ?? [],
  );
  const hasRates = discovery.data?.has("disk.") ?? false;
  const split = view === "each" && devs.length > 0;

  return (
    <>
      {split ? (
        /* 与总体图同一个 grow 外框,切换前后区块高度不变。不给标题行也不给图例:
           每一格自己写着设备名、读数与介质角标,一眼看得出是逐设备 */
        <ChartFrame grow noFrame>
          {(h) => (
            <MemberGrid
              resource={DISK_RESOURCE}
              members={devs}
              height={h}
              cellValue={(d) => {
                const util = latestOf(rings, seriesKey("disk.util", `dev=${d}`));
                const rd = latestOf(rings, seriesKey("disk.read_bytes", `dev=${d}`)) ?? 0;
                const wr = latestOf(rings, seriesKey("disk.write_bytes", `dev=${d}`)) ?? 0;
                return {
                  big: util !== null ? fmtPct(util / 100) : "—",
                  small: `${fmtBytes(rd + wr)}/s`,
                };
              }}
              tagOf={(d) => {
                const disk = (info.data?.disks ?? []).find((x) => x.name === d);
                if (!disk) return null;
                // §5.5:rotational → HDD;名字 nvme 开头 → NVMe;其余 SSD
                return disk.rotational ? "HDD" : d.startsWith("nvme") ? "NVMe" : "SSD";
              }}
            />
          )}
        </ChartFrame>
      ) : (
        hasRates && (
          <AggChart
            groups={[
              {
                name: "读",
                metrics: ["disk.read_bytes"],
                exprs: devs.map((d) => `disk.read_bytes{dev=${d}}`),
              },
              {
                name: "写",
                metrics: ["disk.write_bytes"],
                exprs: devs.map((d) => `disk.write_bytes{dev=${d}}`),
              },
            ]}
            title="磁盘 · 吞吐"
            tone="--disk"
            grow
            range={range}
            onLayer={onLayer}
            unitFmt={(v) => `${fmtBytes(v)}/s`}
          />
        )
      )}
      {discovery.data?.has("psi.io.some") && (
        <div className={s.subRow}>
          <PsiChart
            kind="io"
            title="磁盘 · psi.io（压力）"
            tone="--disk"
            range={range}
            onLayer={onLayer}
          />
        </div>
      )}

      {devs.length > 0 &&
        (() => {
          // 数字区按 §6.2 聚合:饱和度取最大并说明是谁,速率与 IOPS 求和。
          // await 是每次 IO 的平均等待,跨盘求和没有物理意义,取最慢的那块并指名道姓
          const stats: NumItem[] = [];
          let busiest: { d: string; v: number } | null = null;
          let slowest: { d: string; v: number } | null = null;
          let iops: number | null = null;
          for (const d of devs) {
            const u = latestOf(rings, seriesKey("disk.util", `dev=${d}`));
            if (u !== null && (busiest === null || u > busiest.v)) busiest = { d, v: u };
            const aw = latestOf(rings, seriesKey("disk.await", `dev=${d}`));
            if (aw !== null && (slowest === null || aw > slowest.v)) slowest = { d, v: aw };
            const io = latestOf(rings, seriesKey("disk.iops", `dev=${d}`));
            if (io !== null) iops = (iops ?? 0) + io;
          }
          if (busiest !== null)
            stats.push({
              k: "最忙",
              v: fmtPct(busiest.v / 100),
              sub: devs.length > 1 ? busiest.d : undefined,
            });
          const rd = latestSum(rings, "disk.read_bytes");
          const wr = latestSum(rings, "disk.write_bytes");
          if (rd !== null) stats.push({ k: "合计读取", v: `${fmtBytes(rd)}/s` });
          if (wr !== null) stats.push({ k: "合计写入", v: `${fmtBytes(wr)}/s` });
          if (iops !== null) stats.push({ k: "合计 IOPS", v: String(Math.round(iops)) });
          if (slowest !== null)
            stats.push({
              k: devs.length > 1 ? "最慢响应" : "平均响应",
              v: `${slowest.v.toFixed(1)} ms`,
              sub: devs.length > 1 ? slowest.d : undefined,
            });
          const psi = latestOf(rings, seriesKey("psi.io.some", ""));
          if (psi !== null) stats.push({ k: "IO 停滞", v: fmtPct(psi / 100) });

          const facts: NumItem[] = [];
          const disks = info.data?.disks ?? [];
          const media = (x: (typeof disks)[number]) =>
            x.rotational ? "HDD" : x.name.startsWith("nvme") ? "NVMe" : "SSD";
          const one = disks.length === 1 ? disks[0] : undefined;
          if (one) {
            // 只有一块盘时,「合计」就是这一块,直接把这块盘的静态事实摊开
            if (one.model) facts.push({ k: "型号", v: one.model });
            facts.push({ k: "容量", v: fmtBytes(one.size_bytes) });
            facts.push({ k: "介质", v: media(one) });
            if (one.removable) facts.push({ k: "可移动", v: "是" });
            if (one.read_only) facts.push({ k: "只读", v: "是" });
          } else if (disks.length > 0) {
            facts.push({ k: "设备数", v: String(disks.length) });
            facts.push({
              k: "总容量",
              v: fmtBytes(disks.reduce((a, x) => a + x.size_bytes, 0)),
            });
            const kinds = new Map<string, number>();
            for (const x of disks) kinds.set(media(x), (kinds.get(media(x)) ?? 0) + 1);
            facts.push({
              k: "介质",
              v: [...kinds.entries()].map(([m, n]) => `${m} ×${n}`).join(" · "),
            });
            const removable = disks.filter((x) => x.removable).length;
            if (removable > 0) facts.push({ k: "可移动", v: `${removable} 块` });
            const readOnly = disks.filter((x) => x.read_only).length;
            if (readOnly > 0) facts.push({ k: "只读", v: `${readOnly} 块` });
          }
          if (disks.length > 0) {
            const bad = disks.filter((x) => x.smart_healthy === false).length;
            const known = disks.filter(
              (x) => x.smart_healthy !== null && x.smart_healthy !== undefined,
            ).length;
            facts.push({
              k: "SMART",
              v: known === 0 ? "未检测" : bad === 0 ? "全部正常" : `${bad} 块异常`,
            });
          }
          const fsCount = (info.data?.filesystems ?? []).length;
          if (fsCount > 0) facts.push({ k: "挂载点", v: String(fsCount) });
          return <StatFact stats={stats} facts={facts} />;
        })()}

      <MountTable
        filesystems={info.data?.filesystems ?? []}
        rings={rings}
        liveDevs={devs}
        caption="全部挂载点"
      />
    </>
  );
}
