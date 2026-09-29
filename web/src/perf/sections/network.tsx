import { fmtRateBits } from "@/lib/fmt";
import { useDiscovery } from "@/metrics/discovery";
import { latestOf, latestSum, liveMembers, seriesKey, useLive } from "@/metrics/live";
import { MemberGrid, NET_RESOURCE } from "./members";
import {
  AggChart,
  ChartFrame,
  type NumItem,
  type PerfView,
  type SectionProps,
  StatFact,
  useSystemInfo,
} from "./shared";

/* ================= 网络 ================= */

export function NetSection({ range, view, onLayer }: SectionProps & { view: PerfView }) {
  const rings = useLive((st) => st.rings);
  const discovery = useDiscovery();
  const info = useSystemInfo();
  const all = liveMembers(
    rings,
    "net.tx_bytes",
    "iface",
    discovery.data?.members("net.tx_bytes", "iface") ?? [],
  );
  // 有流量的接口排前面；一排 0 b/s 的虚拟接口沉底
  const hasTraffic = (i: string) => {
    for (const m of ["net.rx_bytes", "net.tx_bytes"]) {
      const r = rings.get(seriesKey(m, `iface=${i}`));
      if (r?.v.some((v) => v > 0)) return true;
    }
    return false;
  };
  const ifaces = [...all].sort((a, b) => Number(hasTraffic(b)) - Number(hasTraffic(a)));
  const split = view === "each" && ifaces.length > 0;

  return (
    <>
      {split ? (
        /* 与总体图同一个 grow 外框,切换前后区块高度不变。不给标题行也不给图例:
           每一格自己写着设备名与读数,一眼看得出是逐设备 */
        <ChartFrame grow noFrame>
          {(h) => (
            <MemberGrid
              resource={NET_RESOURCE}
              members={ifaces}
              height={h}
              cellValue={(i) => {
                const rx = latestOf(rings, seriesKey("net.rx_bytes", `iface=${i}`)) ?? 0;
                const tx = latestOf(rings, seriesKey("net.tx_bytes", `iface=${i}`)) ?? 0;
                const errs = latestOf(rings, seriesKey("net.errors", `iface=${i}`));
                return {
                  big: fmtRateBits(rx + tx),
                  small: errs && errs >= 1 ? `错误 ${Math.round(errs)}/s` : undefined,
                };
              }}
              tagOf={(i) => {
                const n = (info.data?.networks ?? []).find((x) => x.name === i);
                return n?.speed_mbps
                  ? n.speed_mbps >= 10_000
                    ? "10G"
                    : `${Math.round(n.speed_mbps / 1000)}G`
                  : null;
              }}
            />
          )}
        </ChartFrame>
      ) : (
        <AggChart
          groups={[
            {
              name: "接收",
              metrics: ["net.rx_bytes"],
              exprs: all.map((i) => `net.rx_bytes{iface=${i}}`),
            },
            {
              name: "发送",
              metrics: ["net.tx_bytes"],
              exprs: all.map((i) => `net.tx_bytes{iface=${i}}`),
            },
          ]}
          title="网络 · 吞吐"
          tone="--net"
          grow
          range={range}
          onLayer={onLayer}
          unitFmt={fmtRateBits}
        />
      )}
      {(() => {
        const stats: NumItem[] = [
          { k: "合计接收", v: fmtRateBits(latestSum(rings, "net.rx_bytes") ?? 0) },
          { k: "合计发送", v: fmtRateBits(latestSum(rings, "net.tx_bytes") ?? 0) },
        ];
        const errs = latestSum(rings, "net.errors");
        if (errs !== null) stats.push({ k: "异常包", v: `${Math.round(errs)}/s` });

        const facts: NumItem[] = [];
        const nets = info.data?.networks ?? [];
        const one = nets.length === 1 ? nets[0] : undefined;
        if (one) {
          // 只有一个接口时,「合计」就是这一个,直接把它的静态事实摊开
          facts.push({ k: "接口", v: one.name });
          facts.push({ k: "载波", v: one.carrier ? "已连接" : "无载波" });
          if (one.speed_mbps) facts.push({ k: "速率", v: `${one.speed_mbps} Mb/s` });
          if (one.mac) facts.push({ k: "MAC", v: one.mac });
          facts.push({ k: "MTU", v: String(one.mtu) });
          if (one.driver) facts.push({ k: "驱动", v: one.driver });
        } else {
          facts.push({ k: "接口数", v: String(all.length) });
          if (nets.length > 0) {
            const up = nets.filter((n) => n.carrier);
            facts.push({ k: "有载波", v: String(up.length) });
            const speeds = new Map<string, number>();
            for (const n of up) {
              if (!n.speed_mbps) continue;
              const label =
                n.speed_mbps >= 1000 ? `${Math.round(n.speed_mbps / 1000)}G` : `${n.speed_mbps}M`;
              speeds.set(label, (speeds.get(label) ?? 0) + 1);
            }
            if (speeds.size > 0)
              facts.push({
                k: "链路",
                v: [...speeds.entries()].map(([sp, n]) => `${sp} ×${n}`).join(" · "),
              });
            const mtus = [...new Set(up.map((n) => n.mtu))];
            if (mtus.length > 0)
              facts.push({ k: "MTU", v: mtus.length === 1 ? String(mtus[0]) : mtus.join(" / ") });
            // 驱动读不到的（虚拟接口）不占位置,只汇总读得到的那些
            const drivers = new Map<string, number>();
            for (const n of nets) {
              if (!n.driver) continue;
              drivers.set(n.driver, (drivers.get(n.driver) ?? 0) + 1);
            }
            if (drivers.size > 0)
              facts.push({
                k: "驱动",
                v: [...drivers.entries()].map(([d, n]) => (n > 1 ? `${d} ×${n}` : d)).join(" · "),
              });
          }
        }
        return <StatFact stats={stats} facts={facts} />;
      })()}
    </>
  );
}
