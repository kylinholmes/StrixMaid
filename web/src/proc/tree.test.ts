import { describe, expect, it } from "vitest";
import { cmpOf, type Proc, treeRows } from "./tree";

function p(pid: number, ppid: number, cpu = 0): Proc {
  return {
    pid,
    ppid,
    name: `p${pid}`,
    uid: 0,
    state: "sleeping",
    cpu_percent: cpu,
    rss_bytes: 0,
    vms_bytes: 0,
    mem_percent: 0,
    threads: 1,
    start_ts: 0,
    nice: 0,
  };
}

/** 1 ── 2 ── 4
 *    └─ 3         5(孤儿:ppid 99 不在列表) */
const FIX = [p(1, 0), p(2, 1), p(4, 2), p(3, 1), p(5, 99)];

describe("treeRows", () => {
  it("默认折叠:根展开一层,更深的收起", () => {
    const rows = treeRows(FIX, new Set());
    // 2 有子进程但在 depth 1,默认折叠 → 4 不出现
    expect(rows.map((r) => [r.p.pid, r.depth, r.children, r.expanded])).toEqual([
      [1, 0, 2, true],
      [2, 1, 1, false],
      [3, 1, 0, false],
      [5, 0, 0, false],
    ]);
  });

  it("toggled 翻转默认态:展开深层、折叠根", () => {
    expect(treeRows(FIX, new Set([2])).map((r) => r.p.pid)).toEqual([1, 2, 4, 3, 5]);
    expect(treeRows(FIX, new Set([1])).map((r) => r.p.pid)).toEqual([1, 5]);
  });

  it("openByDefault 全展开(搜索态),toggled 仍可局部收起", () => {
    const all = treeRows(FIX, new Set(), { openByDefault: true });
    expect(all.map((r) => r.p.pid)).toEqual([1, 2, 4, 3, 5]);
    const some = treeRows(FIX, new Set([2]), { openByDefault: true });
    expect(some.map((r) => r.p.pid)).toEqual([1, 2, 3, 5]);
  });

  it("同级按 cmp 排,父子结构不变", () => {
    const list = [p(1, 0), p(2, 1, 5), p(3, 1, 10), p(4, 2)];
    const rows = treeRows(list, new Set([2]), { cmp: cmpOf({ key: "cpu", order: "desc" }) });
    expect(rows.map((r) => r.p.pid)).toEqual([1, 3, 2, 4]);
  });

  it("环不死循环", () => {
    // 2 ⇄ 3 互为父子的病态数据
    const rows = treeRows([p(2, 3), p(3, 2)], new Set());
    expect(rows.length).toBeGreaterThan(0);
    expect(rows.length).toBeLessThanOrEqual(2);
  });
});

describe("cmpOf", () => {
  it("文本键、数值键与方向", () => {
    const a = { ...p(1, 0, 3), name: "alpha", rss_bytes: 100 };
    const b = { ...p(2, 0, 7), name: "beta", rss_bytes: 50 };
    expect(cmpOf({ key: "name", order: "asc" })(a, b)).toBeLessThan(0);
    expect(cmpOf({ key: "cpu", order: "desc" })(a, b)).toBeGreaterThan(0);
    expect(cmpOf({ key: "mem", order: "desc" })(a, b)).toBeLessThan(0);
  });
});
