/**
 * 有上限的 blob 缓存（插入序 LRU）。
 *
 * 代码梳理（2026-09-25）发现全站 `revokeObjectURL` 出现 0 次：缩略图、
 * 系统图标、按路径图标三处都是无上限的 `Map`，每个条目背后是一个
 * `createObjectURL` 的 blob——浏览一个几千张照片的目录，这些 blob 会在
 * 浏览器里堆到页面关闭为止。object URL 不被 GC 回收（URL 字符串本身就是
 * 强引用），只能显式 revoke，所以上限与回收必须一起做。
 *
 * `get` 会把条目提到最新（Map 保持插入序，删了重插即 touch），逐出的
 * 永远是最久未用的那个。
 */
export class BlobCache<V> {
  private map = new Map<string, V>();

  /**
   * @param cap 条目上限。
   * @param urlOf 从值里取 object URL；返回 `null` 表示该值没有 blob
   *   （负缓存条目），逐出时无需 revoke。
   */
  constructor(
    private readonly cap: number,
    private readonly urlOf: (v: V) => string | null,
  ) {}

  get(key: string): V | undefined {
    const v = this.map.get(key);
    if (v !== undefined) {
      this.map.delete(key);
      this.map.set(key, v);
    }
    return v;
  }

  has(key: string): boolean {
    return this.map.has(key);
  }

  set(key: string, value: V): void {
    const old = this.map.get(key);
    // 同 key 覆盖也要回收旧 blob，否则旧 URL 成了谁都够不着的泄漏。
    if (old !== undefined && old !== value) this.revoke(old);
    this.map.delete(key);
    this.map.set(key, value);
    while (this.map.size > this.cap) {
      const oldest = this.map.keys().next().value;
      if (oldest === undefined) break;
      const evicted = this.map.get(oldest);
      this.map.delete(oldest);
      if (evicted !== undefined) this.revoke(evicted);
    }
  }

  get size(): number {
    return this.map.size;
  }

  clear(): void {
    for (const v of this.map.values()) this.revoke(v);
    this.map.clear();
  }

  private revoke(v: V): void {
    const url = this.urlOf(v);
    // 可选调用：node 测试环境的 URL stub 未必有 revokeObjectURL。
    if (url !== null) URL.revokeObjectURL?.(url);
  }
}
