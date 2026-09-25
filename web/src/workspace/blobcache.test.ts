import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { BlobCache } from "./blobcache";

describe("BlobCache", () => {
  const revoked: string[] = [];

  beforeEach(() => {
    revoked.length = 0;
    // node 环境没有 revokeObjectURL；这里顺便断言逐出时真的调了它。
    vi.stubGlobal("URL", { ...URL, revokeObjectURL: (u: string) => revoked.push(u) });
  });
  afterEach(() => vi.unstubAllGlobals());

  it("超上限按最久未用逐出，并回收其 URL", () => {
    const c = new BlobCache<string>(2, (v) => v);
    c.set("a", "blob:a");
    c.set("b", "blob:b");
    c.set("c", "blob:c");
    expect(c.size).toBe(2);
    expect(c.get("a")).toBeUndefined();
    expect(revoked).toEqual(["blob:a"]);
  });

  it("get 把条目提到最新——被读过的不先被逐出", () => {
    const c = new BlobCache<string>(2, (v) => v);
    c.set("a", "blob:a");
    c.set("b", "blob:b");
    c.get("a"); // touch
    c.set("c", "blob:c");
    expect(c.get("a")).toBe("blob:a");
    expect(revoked).toEqual(["blob:b"]);
  });

  it("同 key 覆盖回收旧 URL——否则旧 blob 谁都够不着", () => {
    const c = new BlobCache<string>(4, (v) => v);
    c.set("a", "blob:old");
    c.set("a", "blob:new");
    expect(revoked).toEqual(["blob:old"]);
    expect(c.get("a")).toBe("blob:new");
    expect(c.size).toBe(1);
  });

  it("负缓存（urlOf 给 null）逐出时不 revoke", () => {
    const c = new BlobCache<string | null>(1, (v) => v);
    c.set("miss", null);
    c.set("hit", "blob:x"); // 把 miss 挤出去
    expect(revoked).toEqual([]);
    expect(c.get("miss")).toBeUndefined();
  });

  it("clear 回收全部", () => {
    const c = new BlobCache<string>(8, (v) => v);
    c.set("a", "blob:a");
    c.set("b", "blob:b");
    c.clear();
    expect(c.size).toBe(0);
    expect(revoked.sort()).toEqual(["blob:a", "blob:b"]);
  });
});
