/** 会话资源的共同生命周期；不依赖 React、API 或功能模块。 */
let scope = new AbortController();
const cleanups = new Set<() => void>();

export function sessionSignal(): AbortSignal {
  return scope.signal;
}

/** 功能模块注册同步回收：清缓存、撤销 Blob URL、丢弃用户状态。 */
export function onSessionReset(cleanup: () => void): () => void {
  cleanups.add(cleanup);
  return () => cleanups.delete(cleanup);
}

export function resetSessionResources(): void {
  const previous = scope;
  scope = new AbortController();
  previous.abort();
  for (const cleanup of cleanups) cleanup();
}
