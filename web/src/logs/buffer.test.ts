import { expect, it } from "vitest";
import { LOG_CAPACITY, prependBounded } from "./buffer";

it("持续滚离顶部时缓冲有界，保留最新批次且不修改入参", () => {
  let pending: number[] = [];
  for (let batch = 0; batch < 100; batch++) {
    const newest = Array.from({ length: 200 }, (_, i) => batch * 200 + 199 - i);
    const before = [...pending];
    const result = prependBounded(newest, pending);
    expect(pending).toEqual(before);
    expect(result.rows.length).toBeLessThanOrEqual(LOG_CAPACITY);
    expect(result.dropped).toBe(Math.max(0, newest.length + pending.length - LOG_CAPACITY));
    pending = result.rows;
  }
  expect(pending[0]).toBe(19999);
  expect(pending.at(-1)).toBe(15000);
});

it("单批超过容量也只保留最新部分", () => {
  const input = Array.from({ length: LOG_CAPACITY * 2 }, (_, i) => i);
  expect(prependBounded(input, [-1])).toEqual({
    rows: input.slice(0, LOG_CAPACITY),
    dropped: LOG_CAPACITY + 1,
  });
});
