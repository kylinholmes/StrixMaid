import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@/api/client";
import type { components } from "@/api/schema";
import {
  Button,
  type Column,
  KeyValueGrid,
  Meter,
  type RunState,
  StatusDot,
  Table,
  TableSkeleton,
} from "@/components";
import { fmtBytes, fmtPct, fmtRateBits, fmtUptime } from "@/lib/fmt";
import { useSession } from "@/session/useSession";
import s from "./PageChrome.module.css";
import { healthQuery, snapshotQuery } from "./Shell";

type UnitSummary = components["schemas"]["UnitSummary"];
type MetricValue = components["schemas"]["MetricValue"];

function infoQuery(enabled: boolean) {
  return {
    queryKey: ["system", "info"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/system/info");
      if (error) throw error;
      return data;
    },
    staleTime: 30_000,
    enabled,
  } as const;
}

function servicesQuery(enabled: boolean) {
  return {
    queryKey: ["services"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/services");
      if (error) throw error;
      return data;
    },
    staleTime: 15_000,
    enabled,
  } as const;
}

/** 快照里取一个指标的合计值（多标签序列求和；找不到返回 null）。 */
function sumMetric(values: readonly MetricValue[] | undefined, metric: string): number | null {
  if (!values) return null;
  let sum = 0;
  let seen = false;
  for (const v of values) {
    if (v.metric === metric && typeof v.value === "number") {
      sum += v.value;
      seen = true;
    }
  }
  return seen ? sum : null;
}

const RUN_STATE: Record<string, RunState> = {
  active: "run",
  reloading: "run",
  activating: "run",
  inactive: "stop",
  deactivating: "stop",
  failed: "fail",
};

const SERVICE_COLUMNS: readonly Column<UnitSummary>[] = [
  {
    key: "name",
    header: "服务",
    mono: true,
    width: "240px",
    render: (u) => u.name,
  },
  {
    key: "state",
    header: "状态",
    width: "110px",
    render: (u) => <StatusDot state={RUN_STATE[u.active_state] ?? "unknown"} />,
  },
  {
    key: "sub",
    header: "细分",
    mono: true,
    dim: true,
    width: "110px",
    render: (u) => u.sub_state,
  },
  {
    key: "desc",
    header: "描述",
    dim: true,
    render: (u) => u.description,
  },
];

export function Overview() {
  const open = useSession((st) => st.status) === "open";
  const qc = useQueryClient();

  const info = useQuery(infoQuery(open));
  const health = useQuery(healthQuery(open));
  const snapshot = useQuery(snapshotQuery(open));
  const services = useQuery(servicesQuery(open));

  const values = snapshot.data?.values;
  const cpu = sumMetric(values, "cpu.usage");
  const memUsed = sumMetric(values, "mem.used");
  const memTotal = sumMetric(values, "mem.total") ?? info.data?.memory.total_bytes ?? null;
  const netRx = sumMetric(values, "net.rx_bytes");
  const netTx = sumMetric(values, "net.tx_bytes");
  const diskRead = sumMetric(values, "disk.read_bytes");
  const diskWrite = sumMetric(values, "disk.write_bytes");

  return (
    <>
      <header className={s.head}>
        <h1>概览</h1>
        <span className={s.spacer} />
        <Button onClick={() => void qc.invalidateQueries()}>刷新</Button>
      </header>

      <div className={s.body}>
        {/* 主机信息：/system/info 到达后填充 */}
        <section className={s.section} aria-label="主机信息">
          {info.data ? (
            <div className={s.arrive}>
              <KeyValueGrid
                items={[
                  { k: "主机名", v: info.data.hostname, mono: true },
                  { k: "系统", v: info.data.os.pretty_name, mono: true },
                  { k: "内核", v: info.data.kernel, mono: true },
                  { k: "运行", v: fmtUptime(info.data.uptime_secs), mono: true },
                  {
                    k: "CPU",
                    v: `${info.data.cpu.model} · ${info.data.cpu.logical_cores} 核`,
                    mono: true,
                  },
                  { k: "内存", v: fmtBytes(info.data.memory.total_bytes), mono: true },
                ]}
              />
            </div>
          ) : (
            <TableSkeleton rows={2} widths={[120, 160, 100, 90]} />
          )}
        </section>

        {/* 资源快照：/metrics/current 到达后填充，此后每 2s 直接重绘（无动画，spec §7） */}
        <section className={s.section} aria-label="资源">
          {values ? (
            <div className={`${s.meters} ${s.arrive}`}>
              <div className={s.meterCell}>
                <div className={s.meterRow}>
                  <b>CPU</b>
                  <span className={s.meterVal}>{cpu === null ? "—" : fmtPct(cpu / 100)}</span>
                </div>
                <Meter value={(cpu ?? 0) / 100} label="CPU 使用率" tone="--cpu" showValue={false} />
              </div>
              <div className={s.meterCell}>
                <div className={s.meterRow}>
                  <b>内存</b>
                  <span className={s.meterVal}>
                    {memUsed !== null && memTotal !== null
                      ? `${fmtBytes(memUsed)} / ${fmtBytes(memTotal)}`
                      : "—"}
                  </span>
                </div>
                <Meter
                  value={memUsed !== null && memTotal ? memUsed / memTotal : 0}
                  label="内存使用率"
                  tone="--mem"
                  showValue={false}
                />
              </div>
              <div className={s.meterCell}>
                <div className={s.meterRow}>
                  <b>磁盘 I/O</b>
                  <span className={s.meterVal}>
                    {diskRead !== null || diskWrite !== null
                      ? `读 ${fmtBytes(diskRead ?? 0)}/s · 写 ${fmtBytes(diskWrite ?? 0)}/s`
                      : "—"}
                  </span>
                </div>
                <Meter value={0} label="磁盘吞吐" tone="--disk" showValue={false} />
              </div>
              <div className={s.meterCell}>
                <div className={s.meterRow}>
                  <b>网络</b>
                  <span className={s.meterVal}>
                    {netRx !== null || netTx !== null
                      ? `↓ ${fmtRateBits(netRx ?? 0)} · ↑ ${fmtRateBits(netTx ?? 0)}`
                      : "—"}
                  </span>
                </div>
                <Meter value={0} label="网络吞吐" tone="--net" showValue={false} />
              </div>
            </div>
          ) : (
            <TableSkeleton rows={1} widths={[140, 140, 140, 140]} />
          )}
        </section>

        {/* 健康：/system/health */}
        <section className={s.section} aria-label="健康">
          <h2 className={s.sectionTitle}>健康</h2>
          {health.data ? (
            <div className={s.arrive}>
              {(health.data.items ?? []).length === 0 ? (
                <div className={s.healthRow}>
                  <StatusDot state="run" label="全部检查通过" />
                </div>
              ) : (
                (health.data.items ?? []).map((it) => (
                  <div key={it.id} className={s.healthRow}>
                    <StatusDot
                      state={
                        it.severity === "critical"
                          ? "fail"
                          : it.severity === "warning"
                            ? "unknown"
                            : "run"
                      }
                      label={it.title}
                    />
                    {it.detail && <span className={s.healthDetail}>{it.detail}</span>}
                  </div>
                ))
              )}
            </div>
          ) : (
            <TableSkeleton rows={1} widths={[220]} />
          )}
        </section>

        {/* 服务：/services */}
        <section className={s.section} aria-label="服务">
          <h2 className={s.sectionTitle}>服务</h2>
          {services.data ? (
            <div className={s.arrive}>
              <Table
                caption="服务列表"
                columns={SERVICE_COLUMNS}
                rows={services.data.slice(0, 12)}
                rowKey={(u) => u.name}
                empty={<span>这台机器上没有列出任何服务。</span>}
              />
            </div>
          ) : (
            <TableSkeleton rows={6} />
          )}
        </section>
      </div>
    </>
  );
}
