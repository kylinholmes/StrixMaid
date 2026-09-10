import { useQuery } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { api } from "@/api/client";
import chrome from "@/app/PageChrome.module.css";
import { capabilitiesQuery } from "@/app/queries";
import { Button, ErrorState, Menu, SearchBox, Segmented, TableSkeleton } from "@/components";
import { cx } from "@/lib/cx";
import { fmtBytes } from "@/lib/fmt";
import { useDismiss } from "@/lib/useDismiss";
import { LogDetail } from "./LogDetail";
import s from "./Logs.module.css";
import { LogTable } from "./LogTable";
import { type LogFilter, type LogPriority, useLogs } from "./useLogs";
import { VacuumDialog } from "./VacuumDialog";

/// 全局日志页回答的是「机器最近有什么不对劲」,默认只看警告及以上(Cockpit 同款思路);
/// info/debug 靠显式下钻,或从服务/进程带着 unit 过滤进来。
const DEFAULT_PRIO = "warning";

const PRIO_FILTERS: { id: string; label: string }[] = [
  { id: "err", label: "错误及以上" },
  { id: "warning", label: "警告及以上" },
  { id: "notice", label: "通知及以上" },
  { id: "info", label: "信息及以上" },
  { id: "debug", label: "含调试" },
  { id: "all", label: "全部级别" },
];

const WINDOWS: { id: string; label: string; secs?: number }[] = [
  { id: "", label: "默认窗口" },
  { id: "1h", label: "最近 1 小时", secs: 3600 },
  { id: "24h", label: "最近 24 小时", secs: 86_400 },
  { id: "7d", label: "最近 7 天", secs: 7 * 86_400 },
];

/** 输入防抖:每个键入都重查(还会重订阅 follow)太浪费。 */
function useDebounced<T>(v: T, ms: number): T {
  const [d, setD] = useState(v);
  useEffect(() => {
    const t = setTimeout(() => setD(v), ms);
    return () => clearTimeout(t);
  }, [v, ms]);
  return d;
}

export function LogsPage() {
  // 筛选状态全在 URL 里:服务页「查看全部日志」等跨页入口靠它(/logs?unit=x&priority=all)
  const [sp, setSp] = useSearchParams();
  const q = sp.get("q") ?? "";
  const unit = sp.get("unit") ?? "";
  const prio = sp.get("priority") ?? DEFAULT_PRIO;
  const boot = sp.get("boot");
  const win = sp.get("win") ?? "";
  const set = useCallback(
    (k: string, v: string | null) => {
      setSp(
        (prev) => {
          const n = new URLSearchParams(prev);
          if (v === null || v === "") n.delete(k);
          else n.set(k, v);
          return n;
        },
        { replace: true },
      );
    },
    [setSp],
  );

  const [follow, setFollow] = useState(true);
  const [selected, setSelected] = useState<string | null>(null);
  const [vacOpen, setVacOpen] = useState(false);
  const [result, setResult] = useState<string | null>(null);

  const dq = useDebounced(q, 400);
  const dunit = useDebounced(unit, 400);

  const filter: LogFilter = useMemo(() => {
    const secs = WINDOWS.find((w) => w.id === win)?.secs;
    return {
      q: dq || undefined,
      unit: dunit || undefined,
      priority: prio === "all" ? undefined : (prio as LogPriority),
      boot: boot ?? undefined,
      since: secs ? Math.floor(Date.now() / 1000) - secs : undefined,
    };
  }, [dq, dunit, prio, boot, win]);

  const live = useLogs(filter, follow);

  const caps = useQuery(capabilitiesQuery());
  const restricted = caps.data?.user?.can_read_journal === false;
  const isMac = caps.data?.identity?.os_id === "macos";

  const usage = useQuery({
    queryKey: ["logs", "usage"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/logs/usage");
      if (error) throw error;
      return data;
    },
    staleTime: 60_000,
    retry: 0,
  });

  const boots = useQuery({
    queryKey: ["logs", "boots"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/logs/boots");
      if (error) throw error;
      return data;
    },
    staleTime: 60_000,
    retry: 0,
  });

  const [prioMenu, setPrioMenu] = useDismiss();
  const [winMenu, setWinMenu] = useDismiss();
  const [bootMenu, setBootMenu] = useDismiss();

  const bootLabel = (idx: number) => (idx === 0 ? "本次启动" : idx === -1 ? "上次启动" : `${idx}`);

  return (
    <>
      <header className={chrome.head}>
        <span className={s.crumb}>
          <b>日志</b>
          {live.loaded && !live.error && (
            <span className={s.crumbSub}>
              已加载 {live.rows.length} 条
              {usage.data?.bytes !== undefined &&
                usage.data?.bytes !== null &&
                ` · 占用 ${fmtBytes(usage.data.bytes)}`}
            </span>
          )}
        </span>
        <span className={chrome.spacer} />
        <SearchBox value={q} onChange={(v) => set("q", v)} placeholder="关键字" label="全文搜索" />
        <input
          className={cx(s.unitInput, unit && s.unitInputOn)}
          value={unit}
          onChange={(e) => set("unit", e.target.value)}
          placeholder="按 unit 过滤"
          aria-label="按 unit 过滤"
        />
        <div className={s.pick}>
          <button
            type="button"
            className={cx(s.pickBtn, prio !== DEFAULT_PRIO && s.pickBtnOn)}
            onClick={(e) => {
              e.stopPropagation();
              setPrioMenu((v) => !v);
            }}
            aria-haspopup="menu"
            aria-expanded={prioMenu}
          >
            {PRIO_FILTERS.find((f) => f.id === prio)?.label ?? prio} ▾
          </button>
          {prioMenu && (
            <Menu
              label="最低级别"
              style={{ position: "absolute", right: 0, top: "100%", zIndex: 30 }}
              items={PRIO_FILTERS.map((f) => ({ id: f.id, label: f.label }))}
              onPick={(id) => {
                set("priority", id === DEFAULT_PRIO ? null : id);
                setPrioMenu(false);
              }}
            />
          )}
        </div>
        <div className={s.pick}>
          <button
            type="button"
            className={cx(s.pickBtn, win && s.pickBtnOn)}
            onClick={(e) => {
              e.stopPropagation();
              setWinMenu((v) => !v);
            }}
            aria-haspopup="menu"
            aria-expanded={winMenu}
            title={isMac ? "macOS 默认只回看最近 5 分钟(统一日志扫描成本高)" : undefined}
          >
            {WINDOWS.find((w) => w.id === win)?.label} ▾
          </button>
          {winMenu && (
            <Menu
              label="时间窗"
              style={{ position: "absolute", right: 0, top: "100%", zIndex: 30 }}
              items={WINDOWS.map((w) => ({
                id: w.id,
                label: w.id === "" && isMac ? "默认(最近 5 分钟)" : w.label,
              }))}
              onPick={(id) => {
                set("win", id);
                setWinMenu(false);
              }}
            />
          )}
        </div>
        {(boots.data?.length ?? 0) > 1 && (
          <div className={s.pick}>
            <button
              type="button"
              className={cx(s.pickBtn, boot && s.pickBtnOn)}
              onClick={(e) => {
                e.stopPropagation();
                setBootMenu((v) => !v);
              }}
              aria-haspopup="menu"
              aria-expanded={bootMenu}
            >
              {boot
                ? (() => {
                    const b = boots.data?.find((x) => x.boot_id === boot);
                    return b ? bootLabel(b.index) : boot.slice(0, 8);
                  })()
                : "全部 boot"}{" "}
              ▾
            </button>
            {bootMenu && (
              <Menu
                label="按 boot 过滤"
                style={{ position: "absolute", right: 0, top: "100%", zIndex: 30 }}
                items={[
                  { id: "", label: "全部 boot" },
                  ...(boots.data ?? []).map((b) => ({
                    id: b.boot_id,
                    label: `${bootLabel(b.index)} · ${new Date(b.first_ts * 1000).toLocaleDateString("zh-CN")}`,
                  })),
                ]}
                onPick={(id) => {
                  set("boot", id === "" ? null : id);
                  setBootMenu(false);
                }}
              />
            )}
          </div>
        )}
        <Segmented
          label="实时"
          value={follow ? "on" : "off"}
          onChange={(v) => setFollow(v === "on")}
          options={[
            { value: "on", label: "跟随" },
            { value: "off", label: "暂停" },
          ]}
        />
        {(usage.data?.modes.length ?? 0) > 0 && (
          <Button size="sm" variant="secondary" onClick={() => setVacOpen(true)}>
            清理…
          </Button>
        )}
      </header>

      {restricted && (
        <div className={s.banner}>
          当前账户不在 systemd-journal / adm 组,只能看到自己的日志——列表不全不是故障。
        </div>
      )}
      {result && (
        <div className={s.resultBar}>
          <span>{result}</span>
          <span className={chrome.spacer} />
          <Button size="sm" iconOnly aria-label="关闭提示" onClick={() => setResult(null)}>
            ×
          </Button>
        </div>
      )}

      <div className={s.page}>
        {live.error ? (
          <div className={s.tableArea}>
            <ErrorState title="日志不可用" detail={live.error} />
          </div>
        ) : !live.loaded ? (
          <div className={s.tableArea}>
            <div style={{ padding: 16 }}>
              <TableSkeleton rows={12} />
            </div>
          </div>
        ) : (
          <LogTable
            rows={live.rows}
            selected={selected}
            onSelect={(c) => setSelected((cur) => (cur === c ? null : c))}
            onPickUnit={(u) => set("unit", u)}
            onAtTop={live.setAtTop}
            onLoadOlder={live.loadOlder}
            loadingOlder={live.loadingOlder}
            hasMore={live.hasMore}
          />
        )}
        {live.pending > 0 && (
          <button type="button" className={s.pill} onClick={live.flush}>
            ↑ {live.pending} 条新日志
          </button>
        )}
        {selected !== null && <LogDetail cursor={selected} onClose={() => setSelected(null)} />}
      </div>

      <VacuumDialog
        open={vacOpen}
        usage={usage.data}
        onClose={() => setVacOpen(false)}
        onDone={(msg) => {
          setResult(msg);
          void usage.refetch();
        }}
      />
    </>
  );
}
