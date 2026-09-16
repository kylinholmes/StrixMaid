import createClient from "openapi-fetch";
import type { paths } from "./schema";

/**
 * Bearer token 由 session store 通过 {@link setAuthToken} 注入，client 不持有状态来源。
 * 不让 client 反向 import session store，否则两个模块循环依赖。
 */
let authToken: string | null = null;

export function setAuthToken(token: string | null): void {
  authToken = token;
}

/**
 * 裸 `fetch` 用的鉴权头。没有会话时返回 `null`，调用方应当直接放弃这次请求。
 *
 * openapi-fetch 只覆盖 openapi.json 里有的路径，返回二进制的端点（进程图标）走裸
 * `fetch`，但用的必须是同一个 token，所以把它以「头」的形态开放出来，
 * 而不是让调用方另存一份 token。
 */
export function authHeaders(): Record<string, string> | null {
  return authToken === null ? null : { Authorization: `Bearer ${authToken}` };
}

/** 唯一的 API client。路径相对当前源，开发时由 Vite 代理到 127.0.0.1:9700。 */
export const api = createClient<paths>();

api.use({
  onRequest({ request }) {
    if (authToken) request.headers.set("Authorization", `Bearer ${authToken}`);
    return request;
  },
});
