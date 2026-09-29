import { fmtBytes, fmtPct, fmtRateBits } from "@/lib/fmt";
import { labelValue, useDiscovery } from "@/metrics/discovery";
import type { RangeKey } from "@/metrics/history";
import { latestOf, seriesKey, useLive } from "@/metrics/live";
import { MountTable } from "../MountTable";
import type { ResourceDef } from "../model";
import s from "../Perf.module.css";

import { AggChart, H_SUB, type NumItem, SeriesChart, StatFact, useSystemInfo } from "./shared";

/* ================= 成员详情 ================= */

/**
 * 成员详情页。同量纲合图：吞吐两条线一张图，有界百分比一张矮图，
 * 计数类进数字区。
 */
export function MemberDetail({
  resource,
  member,
  range,
  onLayer,
}: {
  resource: ResourceDef;
  member: string;
  range: RangeKey;
  onLayer: (layer: string | null) => void;
}) {
  const discovery = useDiscovery();
  const rings = useLive((st) => st.rings);
  const info = useSystemInfo();
  const labelKey = resource.memberLabel ?? "";
  const lbl = `${labelKey}=${member}`;
  const metrics =
    discovery.data?.all
      .filter((m) => labelValue(m.labels, labelKey) === member)
      .map((m) => m.metric)
      .filter((v, i, arr) => arr.indexOf(v) === i) ?? [];

  if (metrics.length === 0) {
    return <p className={s.note}>没有可用序列。</p>;
  }

  const has = (m: string) => metrics.includes(m);
  const num = (m: string) => latestOf(rings, seriesKey(m, lbl));

  const charts: React.ReactNode[] = [];
  const numberItems: NumItem[] = [];
  const facts: NumItem[] = [];

  if (resource.id === "net") {
    if (has("net.rx_bytes"))
      charts.push(
        <SeriesChart
          key="thru"
          expr={`net.rx_bytes{${lbl}}`}
          metric="net.rx_bytes"
          labels={lbl}
          title={`${member} · 吞吐`}
          tone="--net"
          grow
          range={range}
          onLayer={onLayer}
          unitFmt={fmtRateBits}
          secondary={
            has("net.tx_bytes")
              ? {
                  expr: `net.tx_bytes{${lbl}}`,
                  metric: "net.tx_bytes",
                  labels: lbl,
                  name: "net.tx_bytes",
                }
              : undefined
          }
        />,
      );
    const rx = num("net.rx_bytes");
    const tx = num("net.tx_bytes");
    const er = num("net.errors");
    if (rx !== null) numberItems.push({ k: "接收", v: fmtRateBits(rx) });
    if (tx !== null) numberItems.push({ k: "发送", v: fmtRateBits(tx) });
    if (er !== null) numberItems.push({ k: "错误", v: `${Math.round(er)}/s` });
    const n = (info.data?.networks ?? []).find((x) => x.name === member);
    if (n) {
      facts.push({ k: "链路", v: n.carrier ? "已连接" : "无载波" });
      if (n.speed_mbps) facts.push({ k: "速率", v: `${n.speed_mbps} Mb/s` });
      if (n.mac) facts.push({ k: "MAC", v: n.mac });
      if (n.mtu) facts.push({ k: "MTU", v: String(n.mtu) });
      // 驱动:虚拟接口（bridge、tun、Hyper-V 虚拟交换机）读不到,留空不占位
      if (n.driver) facts.push({ k: "驱动", v: n.driver });
      for (const a of (n.addrs ?? []).slice(0, 4)) facts.push({ k: "地址", v: a });
    }
  } else if (resource.id === "disk") {
    if (has("disk.read_bytes"))
      charts.push(
        <SeriesChart
          key="thru"
          expr={`disk.read_bytes{${lbl}}`}
          metric="disk.read_bytes"
          labels={lbl}
          title={`${member} · 吞吐`}
          tone="--disk"
          grow
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => `${fmtBytes(v)}/s`}
          secondary={
            has("disk.write_bytes")
              ? {
                  expr: `disk.write_bytes{${lbl}}`,
                  metric: "disk.write_bytes",
                  labels: lbl,
                  name: "disk.write_bytes",
                }
              : undefined
          }
        />,
      );
    if (has("disk.util"))
      charts.push(
        <SeriesChart
          key="util"
          expr={`disk.util{${lbl}}`}
          metric="disk.util"
          labels={lbl}
          title={`${member} · disk.util`}
          tone="--disk"
          yMax={100}
          height={H_SUB}
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => fmtPct(v / 100)}
        />,
      );
    const util = num("disk.util");
    if (util !== null) numberItems.push({ k: "活动时间", v: fmtPct(util / 100) });
    const rd = num("disk.read_bytes");
    const wr = num("disk.write_bytes");
    if (rd !== null) numberItems.push({ k: "读取", v: `${fmtBytes(rd)}/s` });
    if (wr !== null) numberItems.push({ k: "写入", v: `${fmtBytes(wr)}/s` });
    const iops = num("disk.iops");
    const aw = num("disk.await");
    if (iops !== null) numberItems.push({ k: "IOPS", v: String(Math.round(iops)) });
    if (aw !== null) numberItems.push({ k: "平均响应", v: `${aw.toFixed(1)} ms` });
    const d = (info.data?.disks ?? []).find((x) => x.name === member);
    if (d) {
      facts.push(
        { k: "型号", v: d.model || "—" },
        { k: "容量", v: fmtBytes(d.size_bytes) },
        {
          k: "介质",
          v: d.rotational ? "HDD" : member.startsWith("nvme") ? "NVMe" : "SSD",
        },
      );
      if (d.removable) facts.push({ k: "可移动", v: "是" });
      if (d.read_only) facts.push({ k: "只读", v: "是" });
      facts.push({
        k: "SMART",
        v:
          d.smart_healthy === null || d.smart_healthy === undefined
            ? "未检测"
            : d.smart_healthy
              ? "健康"
              : "异常",
      });
      const mine = (info.data?.filesystems ?? []).filter((f) => f.backing_dev === member);
      if (mine.length > 0) facts.push({ k: "挂载点", v: String(mine.length) });
    }
  } else if (resource.id === "gpu") {
    if (has("gpu.usage"))
      charts.push(
        <SeriesChart
          key="usage"
          expr={`gpu.usage{${lbl}}`}
          metric="gpu.usage"
          labels={lbl}
          title={`${member} · gpu.usage`}
          tone="--gpu"
          yMax={100}
          grow
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => fmtPct(v / 100)}
        />,
      );
    // 引擎细分只在单卡视图下有意义：整组取最大会把「哪张卡在忙」这一层抹掉。
    const engines = (discovery.data?.all ?? [])
      .filter((m) => m.metric === "gpu.engine.usage" && labelValue(m.labels, "gpu") === member)
      .map((m) => labelValue(m.labels, "engine"))
      .filter((e): e is string => e !== null)
      .filter((e, i, a) => a.indexOf(e) === i);
    if (engines.length > 0)
      charts.push(
        <AggChart
          key="engines"
          groups={engines.map((e) => ({
            name: e,
            metrics: ["gpu.engine.usage"],
            exprs: [`gpu.engine.usage{engine=${e},${lbl}}`],
            // 同一个 metric 分不出组，得连 engine 和 gpu 一起认
            match: (key: string) =>
              key.startsWith("gpu.engine.usage|") &&
              key.includes(`engine=${e}`) &&
              key.includes(lbl),
          }))}
          agg="max"
          title={`${member} · 引擎细分`}
          tone="--gpu"
          yMax={100}
          height={H_SUB}
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => fmtPct(v / 100)}
        />,
      );
    if (has("gpu.mem_used"))
      charts.push(
        <SeriesChart
          key="mem"
          expr={`gpu.mem_used{${lbl}}`}
          metric="gpu.mem_used"
          labels={lbl}
          title={`${member} · gpu.mem_used`}
          tone="--gpu"
          height={H_SUB}
          range={range}
          onLayer={onLayer}
          unitFmt={fmtBytes}
        />,
      );
    const gu = num("gpu.usage");
    if (gu !== null) numberItems.push({ k: "利用率", v: fmtPct(gu / 100) });
    const gm = num("gpu.mem_used");
    const gmt = num("gpu.mem_total");
    if (gm !== null)
      numberItems.push({
        k: "显存",
        v: fmtBytes(gm),
        sub: gmt !== null ? `/ ${fmtBytes(gmt)}` : undefined,
      });
    const gt = num("gpu.temp");
    if (gt !== null) numberItems.push({ k: "温度", v: `${Math.round(gt)} °C` });
    // member 是 gpu 标签值（卡名）,按 card 名对齐;对不上宁可不显示,不能拿别的卡凑数
    const g = (info.data?.gpus ?? []).find((x) => x.card === member);
    if (g) {
      facts.push({ k: "型号", v: g.model || "—" });
      if (g.driver) facts.push({ k: "驱动", v: g.driver });
      if (g.vram_bytes) facts.push({ k: "显存", v: fmtBytes(g.vram_bytes) });
      if (g.bus) facts.push({ k: "总线", v: g.bus });
    }
  } else {
    for (const metric of metrics) {
      charts.push(
        <SeriesChart
          key={metric}
          expr={`${metric}{${lbl}}`}
          metric={metric}
          labels={lbl}
          title={`${member} · ${metric}`}
          tone={resource.tone}
          height={H_SUB}
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => v.toFixed(1)}
        />,
      );
    }
  }

  return (
    <>
      {charts}
      {resource.id === "disk" && (
        /* 这块盘上的挂载点（08 §6.3 的第二处）。挂载点列在它所在的盘里,
           「共享设备级 IO 计数器」就不言自明。自己页内不再跳自己,行不可点。 */
        <MountTable
          filesystems={(info.data?.filesystems ?? []).filter((f) => f.backing_dev === member)}
          rings={rings}
          liveDevs={[]}
          caption="这块盘上的挂载点"
          defaultOpen
        />
      )}
      <StatFact stats={numberItems} facts={facts} />
    </>
  );
}
