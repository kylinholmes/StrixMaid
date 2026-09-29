import { create } from "zustand";
import { api, authHeaders, setAuthToken } from "@/api/client";
import { wsClient } from "@/lib/ws";
import { startLive, stopLive } from "@/metrics/live";
import { sessionSignal } from "./lifecycle";

/** 会话里留存的最小身份。gid 等细节用不上就不存。 */
export interface SessionUser {
  username: string;
  uid: number;
  groups: readonly string[];
}

/**
 * - `boot`：应用刚启动，正在用本地 token 换会话信息，门保持合上；
 * - `locked`：无有效会话，门合上，显示登录表单；
 * - `open`：已认证，门打开。
 */
export type SessionStatus = "boot" | "locked" | "open";

interface SessionState {
  epoch: number;
  status: SessionStatus;
  user: SessionUser | null;
  /** 启动时恢复：本地有 token 就问一次 `/auth/session`，无效即清。 */
  restore: () => Promise<void>;
  /** 认证完成（LoginForm 拿到 `status: "complete"` 后调用）。 */
  complete: (token: string, user: SessionUser) => void;
  /** 锁定：撤销服务端会话 + 清本地 token。失败也照样锁——本地丢弃即视为锁上。 */
  lock: () => Promise<void>;
}

import { SESSION_TOKEN_KEY as TOKEN_KEY } from "./token";

export const useSession = create<SessionState>((set) => ({
  epoch: 0,
  status: "boot",
  user: null,

  restore: async () => {
    const saved = globalThis.localStorage?.getItem(TOKEN_KEY) ?? null;
    if (!saved) {
      setAuthToken(null);
      set({ status: "locked", user: null });
      return;
    }
    setAuthToken(saved);
    const scope = sessionSignal();
    const { data } = await api.GET("/api/v1/auth/session").catch(() => ({ data: undefined }));
    if (scope.aborted) return;
    if (data) {
      wsClient.connect(saved);
      startLive();
      set({
        status: "open",
        epoch: useSession.getState().epoch + 1,
        user: { username: data.username, uid: data.uid, groups: data.groups ?? [] },
      });
    } else {
      setAuthToken(null);
      globalThis.localStorage?.removeItem(TOKEN_KEY);
      set({ status: "locked" });
    }
  },

  complete: (token, user) => {
    stopLive();
    wsClient.close();
    setAuthToken(token);
    globalThis.localStorage?.setItem(TOKEN_KEY, token);
    wsClient.connect(token);
    startLive();
    set((state) => ({ status: "open", user, epoch: state.epoch + 1 }));
  },

  lock: async () => {
    // 先捕获旧凭据再同步关门；网络撤销不能阻塞本地锁定。
    const headers = authHeaders();
    stopLive();
    wsClient.close();
    setAuthToken(null);
    globalThis.localStorage?.removeItem(TOKEN_KEY);
    set({ status: "locked", user: null });
    if (headers) {
      // 不走会话中间件：新登录不能覆盖旧 token，也不能取消旧会话的撤销。
      await fetch("/api/v1/auth/logout", {
        method: "POST",
        headers,
        signal: AbortSignal.timeout(5_000),
      }).catch(() => undefined);
    }
  },
}));
