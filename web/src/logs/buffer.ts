/** 日志由新到旧；每个缓冲最多留 5,000 条，拼接时只复制会保留的部分。 */
export const LOG_CAPACITY = 5_000;

export function prependBounded<T>(newest: readonly T[], previous: readonly T[]) {
  const head = newest.slice(0, LOG_CAPACITY);
  const rows = head.concat(previous.slice(0, LOG_CAPACITY - head.length));
  return { rows, dropped: newest.length + previous.length - rows.length };
}
