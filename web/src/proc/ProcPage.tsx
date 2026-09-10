import { useMemo, useState } from "react";
import chrome from "@/app/PageChrome.module.css";
import { Menu, SearchBox, TableSkeleton } from "@/components";
import { cx } from "@/lib/cx";
import { useDismiss } from "@/lib/useDismiss";
import { latestOf, seriesKey, useLive } from "@/metrics/live";
import s from "./Proc.module.css";
import { ProcDetail } from "./ProcDetail";
import { ProcTable } from "./ProcTable";
import { cmpOf, type SortState, type TreeRow, treeRows } from "./tree";
import { type ProcQuery, useProcLive } from "./useProcLive";

export function ProcPage() {
  const [q, setQ] = useState("");
  const [user, setUser] = useState<string | null>(null);
  const [sort, setSort] = useState<SortState>({ key: "cpu", order: "desc" });
  const [selected, setSelected] = useState<number | null>(null);
  const [toggled, setToggled] = useState<ReadonlySet<number>>(new Set());
  const rings = useLive((st) => st.rings);

  // 排序不进查询——纯客户端排,免得改个排序就重订阅、整表闪一下
  const query: ProcQuery = {
    q: q || undefined,
    user: user ?? undefined,
    tree: true,
  };
  const live = useProcLive(query);

  const display: TreeRow[] = useMemo(
    // 搜索时全展开,否则命中项会藏在默认折叠的子树里
    () => treeRows(live.rows, toggled, { cmp: cmpOf(sort), openByDefault: q !== "" }),
    [live.rows, toggled, sort, q],
  );

  // 页头汇总与表头总量
  const running = live.rows.filter((p) => p.state === "running").length;
  const threads = live.rows.reduce((a, p) => a + p.threads, 0);
  const cpuTotal = latestOf(rings, seriesKey("cpu.usage", ""));
  const memUsed = latestOf(rings, seriesKey("mem.used", ""));
  const memTotal = latestOf(rings, seriesKey("mem.total", ""));
  const memPct = memUsed !== null && memTotal ? (memUsed / memTotal) * 100 : null;

  // 用户筛选:候选 = 当前列表里出现过的用户名
  const users = useMemo(
    () => [...new Set(live.rows.map((p) => p.user).filter((u): u is string => !!u))].sort(),
    [live.rows],
  );
  const [userMenu, setUserMenu] = useDismiss();

  return (
    <>
      <header className={chrome.head}>
        <span className={s.crumb}>
          <b>进程</b>
          {live.loaded && (
            <span className={s.crumbSub}>
              {live.rows.length} 进程 · {running} 运行中 · {threads} 线程
              {!live.up && " · 轮询兜底"}
            </span>
          )}
        </span>
        <span className={chrome.spacer} />
        <SearchBox value={q} onChange={setQ} placeholder="名称或命令行" label="筛选进程" />
        <div className={s.userPick}>
          <button
            type="button"
            className={cx(s.userBtn, user && s.userBtnOn)}
            onClick={(e) => {
              e.stopPropagation();
              setUserMenu((v) => !v);
            }}
            aria-haspopup="menu"
            aria-expanded={userMenu}
          >
            {user ?? "全部用户"} ▾
          </button>
          {userMenu && (
            <Menu
              label="按用户筛选"
              style={{ position: "absolute", right: 0, top: "100%", zIndex: 30 }}
              items={[{ id: "", label: "全部用户" }, ...users.map((u) => ({ id: u, label: u }))]}
              onPick={(id) => {
                setUser(id === "" ? null : id);
                setUserMenu(false);
              }}
            />
          )}
        </div>
      </header>

      <div className={s.page}>
        {!live.loaded ? (
          <div className={s.tableArea}>
            <div style={{ padding: 16 }}>
              <TableSkeleton rows={12} />
            </div>
          </div>
        ) : (
          <ProcTable
            rows={display}
            sort={sort}
            onSort={setSort}
            selected={selected}
            onSelect={(pid) => setSelected((cur) => (cur === pid ? null : pid))}
            onToggle={(pid) =>
              setToggled((cur) => {
                const next = new Set(cur);
                if (next.has(pid)) next.delete(pid);
                else next.add(pid);
                return next;
              })
            }
            cpuTotal={cpuTotal}
            memPct={memPct}
          />
        )}
        {selected !== null && <ProcDetail pid={selected} onClose={() => setSelected(null)} />}
      </div>
    </>
  );
}
