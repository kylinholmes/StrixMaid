import { useQuery } from "@tanstack/react-query";
import { api } from "@/api/client";
import { EmptyState, ErrorState, TableSkeleton, Tag } from "@/components";
import { cx } from "@/lib/cx";
import s from "./Svc.module.css";
import type { Scope, Timer } from "./useUnits";

const SOURCE_LABEL: Record<Timer["source"], string> = {
  systemd_timer: "systemd",
  launchd: "launchd",
};

function fmtAbs(ts: number): string {
  return new Date(ts * 1000).toLocaleString("zh-CN", { hour12: false });
}

/** 相对时刻:未来「N 后」,过去「N 前」。表格里看趋势用,精确值在 title。 */
export function fmtRel(ts: number, now: number): string {
  const d = ts - now;
  const abs = Math.abs(d);
  const unit =
    abs < 90 * 60
      ? `${Math.max(1, Math.round(abs / 60))} 分钟`
      : abs < 36 * 3600
        ? `${Math.round(abs / 3600)} 小时`
        : `${Math.round(abs / 86_400)} 天`;
  return d >= 0 ? `${unit}后` : `${unit}前`;
}

export function TimerTable({
  scope,
  q,
  selected,
  onSelect,
}: {
  scope: Scope;
  q: string;
  selected: string | null;
  onSelect: (unit: string) => void;
}) {
  const timers = useQuery({
    queryKey: ["svc", "timers", scope],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/services/timers", {
        params: { query: { scope } },
      });
      if (error) throw error;
      return data;
    },
    refetchInterval: 30_000,
  });

  if (timers.isPending) {
    return (
      <div className={s.tableArea}>
        <div style={{ padding: 16 }}>
          <TableSkeleton rows={8} />
        </div>
      </div>
    );
  }
  if (timers.isError) {
    const e = timers.error as { message?: string; detail?: string };
    return (
      <div className={s.tableArea}>
        <ErrorState title={e.message ?? "定时任务拉取失败"} detail={e.detail} />
      </div>
    );
  }

  const kw = q.trim().toLowerCase();
  const rows = timers.data.filter(
    (t) =>
      !kw ||
      t.name.toLowerCase().includes(kw) ||
      (t.target ?? "").toLowerCase().includes(kw) ||
      t.schedule.some((l) => l.toLowerCase().includes(kw)),
  );
  if (rows.length === 0) {
    return (
      <div className={s.tableArea}>
        <EmptyState
          title={kw ? "没有匹配的定时任务" : "没有定时任务"}
          detail={kw ? undefined : "本作用域下没有 systemd timer / launchd 定时 job。"}
        />
      </div>
    );
  }
  const now = Date.now() / 1000;

  return (
    <div className={s.tableArea}>
      <table className={s.tbl}>
        <thead>
          <tr>
            <th>
              <span className={s.thBtn}>名称</span>
            </th>
            <th>
              <span className={s.thBtn}>来源</span>
            </th>
            <th>
              <span className={s.thBtn}>调度</span>
            </th>
            <th>
              <span className={s.thBtn}>下次运行</span>
            </th>
            <th>
              <span className={s.thBtn}>上次运行</span>
            </th>
            <th>
              <span className={s.thBtn}>目标</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {rows.map((t, i) => (
            <tr
              key={t.name}
              className={cx(s.row, i % 2 === 1 && s.alt, selected === t.name && s.rowOn)}
              onClick={() => onSelect(t.name)}
            >
              <td className={s.name}>
                <span className={s.nameWrap}>
                  {/* active 是三态:false 才标「未生效」,null = 读不到加载状态,不断言 */}
                  <b className={cx(t.active === false && s.dim)}>{t.name}</b>
                  {t.active === false && <span className={s.desc}>未生效</span>}
                </span>
              </td>
              <td>
                <Tag>{SOURCE_LABEL[t.source]}</Tag>
              </td>
              <td className={s.sched}>
                {/* 空数组 = 拿不到规则(降级路径),画「—」不编数据 */}
                <span className={s.schedText} title={t.schedule.join("\n")}>
                  {t.schedule.length > 0 ? t.schedule.join("; ") : "—"}
                </span>
              </td>
              <td
                className={s.mono}
                title={
                  t.next_ts !== undefined && t.next_ts !== null
                    ? fmtAbs(t.next_ts)
                    : "推算不了:定时器未生效,或 StartInterval 取决于装载时刻"
                }
              >
                {t.next_ts !== undefined && t.next_ts !== null ? fmtRel(t.next_ts, now) : "—"}
              </td>
              <td
                className={cx(s.mono, s.dim)}
                title={
                  t.last_ts !== undefined && t.last_ts !== null
                    ? fmtAbs(t.last_ts)
                    : "来源不记录上次触发"
                }
              >
                {t.last_ts !== undefined && t.last_ts !== null ? fmtRel(t.last_ts, now) : "—"}
              </td>
              <td className={cx(s.mono, s.dim)}>{t.target ?? "—"}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
