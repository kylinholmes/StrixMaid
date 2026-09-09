import type { components } from "@/api/schema";

export type Proc = components["schemas"]["ProcessSummary"];

export interface TreeRow {
  p: Proc;
  /** 缩进层级,根为 0 */
  depth: number;
  /** 直接子进程数(0 = 叶子,不画折叠钮) */
  children: number;
  /** 本行当前是否展开(叶子恒为 false) */
  expanded: boolean;
}

/** 表头排序状态。树模式下作用于**同级兄弟**,父子结构不变。 */
export interface SortState {
  key: "name" | "pid" | "user" | "cpu" | "mem" | "io" | "threads" | "nice" | "start";
  order: "asc" | "desc";
}

/** 排序比较器。全部客户端排:树模式下服务端排序对同级无意义。 */
export function cmpOf(sort: SortState): (a: Proc, b: Proc) => number {
  const dir = sort.order === "asc" ? 1 : -1;
  const by = (f: (p: Proc) => number) => (a: Proc, b: Proc) => (f(a) - f(b)) * dir;
  switch (sort.key) {
    case "name":
      return (a, b) => a.name.localeCompare(b.name) * dir;
    case "user":
      return (a, b) => (a.user ?? String(a.uid)).localeCompare(b.user ?? String(b.uid)) * dir;
    case "pid":
      return by((p) => p.pid);
    case "cpu":
      return by((p) => p.cpu_percent);
    case "mem":
      return by((p) => p.rss_bytes);
    case "io":
      return by((p) => (p.io_read_rate ?? 0) + (p.io_write_rate ?? 0));
    case "threads":
      return by((p) => p.threads);
    case "nice":
      return by((p) => p.nice);
    case "start":
      return by((p) => p.start_ts);
  }
}

export interface TreeOpts {
  /** 同级排序;不传沿用服务端 DFS 给出的顺序 */
  cmp?: (a: Proc, b: Proc) => number;
  /** 非根节点的默认展开态。搜索时置 true,否则命中项会被默认折叠藏掉。根恒默认展开。 */
  openByDefault?: boolean;
}

/**
 * 平铺数组 → 树的展示行(DFS)。
 *
 * 默认折叠:根(depth 0)默认展开,更深的节点默认折叠——首屏是一层
 * 顶级进程,像任务管理器;`toggled` 里的 pid **翻转**各自的默认态。
 * ppid 不在列表里的(祖先被内核回收/被筛掉)按根处理,树不断链。
 */
export function treeRows(
  list: readonly Proc[],
  toggled: ReadonlySet<number>,
  opts: TreeOpts = {},
): TreeRow[] {
  const byParent = new Map<number, Proc[]>();
  const pids = new Set(list.map((p) => p.pid));
  const roots: Proc[] = [];
  for (const p of list) {
    if (p.ppid !== p.pid && pids.has(p.ppid)) {
      const kids = byParent.get(p.ppid);
      if (kids) kids.push(p);
      else byParent.set(p.ppid, [p]);
    } else {
      roots.push(p);
    }
  }
  if (opts.cmp) {
    roots.sort(opts.cmp);
    for (const kids of byParent.values()) kids.sort(opts.cmp);
  }

  const out: TreeRow[] = [];
  const visited = new Set<number>();
  // emit=false 只标记不输出:被折叠的子树要整棵吞掉,不能漏到兜底循环里变成根
  const walk = (p: Proc, depth: number, emit: boolean) => {
    if (visited.has(p.pid)) return; // 病态数据里的环,宁可少画不死循环
    visited.add(p.pid);
    const kids = byParent.get(p.pid) ?? [];
    const open =
      kids.length > 0 && (depth === 0 || opts.openByDefault === true) !== toggled.has(p.pid);
    if (emit) out.push({ p, depth, children: kids.length, expanded: open });
    for (const k of kids) walk(k, depth + 1, emit && open);
  };
  for (const r of roots) walk(r, 0, true);
  // 兜底:环等病态数据可能没有根,任何没被覆盖的行也要出现(walk 会跳过已访问的)
  for (const p of list) walk(p, 0, true);
  return out;
}
