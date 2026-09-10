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

/** 唯一的 API client。路径相对当前源，开发时由 Vite 代理到 127.0.0.1:9700。 */
export const api = createClient<paths>();

api.use({
  onRequest({ request }) {
    if (authToken) request.headers.set("Authorization", `Bearer ${authToken}`);
    return request;
  },
});
