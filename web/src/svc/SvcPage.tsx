import { useQuery } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import { useSearchParams } from "react-router-dom";
import chrome from "@/app/PageChrome.module.css";
import { capabilitiesQuery } from "@/app/queries";
import { ErrorState, Menu, SearchBox, Segmented, TableSkeleton } from "@/components";
import { cx } from "@/lib/cx";
import { useDismiss } from "@/lib/useDismiss";
import s from "./Svc.module.css";
import { TimerTable } from "./TimerTable";
import { UnitDetail } from "./UnitDetail";
import { type UnitSort, UnitTable, unitCmp } from "./UnitTable";
import { type Scope, useUnits } from "./useUnits";

/** systemd 的 unit 后缀。跨页链接给的名字缺后缀时(macOS 日志的 subsystem)补 `.service`。 */
const UNIT_SUFFIXES = new Set([
  "service",
  "socket",
  "timer",
  "target",
  "mount",
  "automount",
  "path",
  "slice",
  "scope",
  "swap",
  "device",
]);

function normalizeUnit(u: string): string {
  const last = u.split(".").pop() ?? "";
  return UNIT_SUFFIXES.has(last) ? u : `${u}.service`;
}

const STATE_FILTERS = [
  { id: "", label: "全部状态" },
  { id: "active", label: "运行中" },
  { id: "failed", label: "失败" },
  { id: "inactive", label: "未运行" },
] as const;

export function SvcPage() {
  const [tab, setTab] = useState<"units" | "timers">("units");
  const [scope, setScope] = useState<Scope>("system");
  const [q, setQ] = useState("");
  const [typeF, setTypeF] = useState<string | null>(null);
  const [stateF, setStateF] = useState<string | null>(null);
  // 默认按状态升序:failed 浮最上,任务管理器不会把出事的埋在第 300 行
  const [sort, setSort] = useState<UnitSort>({ key: "state", order: "asc" });
  const [selected, setSelected] = useState<string | null>(null);

  // 跨页深链:/services?unit=x 直接打开该 unit 的抽屉(进程页「服务单元」用)
  const [sp] = useSearchParams();
  useEffect(() => {
    const u = sp.get("unit");
    if (u) setSelected(normalizeUnit(u));
  }, [sp]);

  const caps = useQuery(capabilitiesQuery());
  const userUnits = caps.data?.system.user_units ?? false;
  const live = useUnits(scope);

  // 类型候选 = 数据里实际出现过的类型
  const types = useMemo(() => [...new Set(live.rows.map((u) => u.unit_type))].sort(), [live.rows]);

  const filtered = useMemo(() => {
    const kw = q.trim().toLowerCase();
    return live.rows
      .filter(
        (u) =>
          (!typeF || u.unit_type === typeF) &&
          (!stateF || u.active_state === stateF) &&
          (!kw || u.name.toLowerCase().includes(kw) || u.description.toLowerCase().includes(kw)),
      )
      .sort(unitCmp(sort));
  }, [live.rows, q, typeF, stateF, sort]);

  const running = live.rows.filter((u) => u.active_state === "active").length;
  const failed = live.rows.filter((u) => u.active_state === "failed").length;

  const [typeMenu, setTypeMenu] = useDismiss();
  const [stateMenu, setStateMenu] = useDismiss();

  return (
    <>
      <header className={chrome.head}>
        <span className={s.crumb}>
          <b>服务</b>
          {live.loaded && !live.error && (
            <span className={s.crumbSub}>
              {live.rows.length} unit · {running} 运行中
              {failed > 0 && ` · ${failed} 失败`}
            </span>
          )}
        </span>
        <span className={chrome.spacer} />
        <SearchBox value={q} onChange={setQ} placeholder="名称或描述" label="筛选" />
        {tab === "units" && (
          <>
            <div className={s.pick}>
              <button
                type="button"
                className={cx(s.pickBtn, typeF && s.pickBtnOn)}
                onClick={(e) => {
                  e.stopPropagation();
                  setTypeMenu((v) => !v);
                }}
                aria-haspopup="menu"
                aria-expanded={typeMenu}
              >
                {typeF ?? "全部类型"} ▾
              </button>
              {typeMenu && (
                <Menu
                  label="按类型筛选"
                  style={{ position: "absolute", right: 0, top: "100%", zIndex: 30 }}
                  items={[
                    { id: "", label: "全部类型" },
                    ...types.map((t) => ({ id: t, label: t })),
                  ]}
                  onPick={(id) => {
                    setTypeF(id === "" ? null : id);
                    setTypeMenu(false);
                  }}
                />
              )}
            </div>
            <div className={s.pick}>
              <button
                type="button"
                className={cx(s.pickBtn, stateF && s.pickBtnOn)}
                onClick={(e) => {
                  e.stopPropagation();
                  setStateMenu((v) => !v);
                }}
                aria-haspopup="menu"
                aria-expanded={stateMenu}
              >
                {STATE_FILTERS.find((f) => f.id === stateF)?.label ?? "全部状态"} ▾
              </button>
              {stateMenu && (
                <Menu
                  label="按状态筛选"
                  style={{ position: "absolute", right: 0, top: "100%", zIndex: 30 }}
                  items={STATE_FILTERS.map((f) => ({ id: f.id, label: f.label }))}
                  onPick={(id) => {
                    setStateF(id === "" ? null : id);
                    setStateMenu(false);
                  }}
                />
              )}
            </div>
          </>
        )}
        {userUnits && (
          <Segmented
            label="作用域"
            value={scope}
            onChange={(v: Scope) => {
              setScope(v);
              setSelected(null);
            }}
            options={[
              { value: "system", label: "系统" },
              { value: "user", label: "用户" },
            ]}
          />
        )}
        <Segmented
          label="内容"
          value={tab}
          onChange={setTab}
          options={[
            { value: "units", label: "服务" },
            { value: "timers", label: "定时任务" },
          ]}
        />
      </header>

      <div className={s.page}>
        {tab === "timers" ? (
          <TimerTable scope={scope} q={q} selected={selected} onSelect={setSelected} />
        ) : live.error ? (
          <div className={s.tableArea}>
            <ErrorState title="服务列表不可用" detail={live.error} />
          </div>
        ) : !live.loaded ? (
          <div className={s.tableArea}>
            <div style={{ padding: 16 }}>
              <TableSkeleton rows={12} />
            </div>
          </div>
        ) : (
          <UnitTable
            rows={filtered}
            sort={sort}
            onSort={setSort}
            selected={selected}
            onSelect={(name) => setSelected((cur) => (cur === name ? null : name))}
          />
        )}
        {selected !== null && (
          <UnitDetail unit={selected} scope={scope} onClose={() => setSelected(null)} />
        )}
      </div>
    </>
  );
}
