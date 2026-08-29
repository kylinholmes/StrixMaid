import { create } from "zustand";
import { api, setAuthToken } from "@/api/client";

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
  status: SessionStatus;
  user: SessionUser | null;
  /** 启动时恢复：本地有 token 就问一次 `/auth/session`，无效即清。 */
  restore: () => Promise<void>;
  /** 认证完成（LoginForm 拿到 `status: "complete"` 后调用）。 */
  complete: (token: string, user: SessionUser) => void;
  /** 锁定：撤销服务端会话 + 清本地 token。失败也照样锁——本地丢弃即视为锁上。 */
  lock: () => Promise<void>;
}

const TOKEN_KEY = "strixmaid.session.token";

export const useSession = create<SessionState>((set) => ({
  status: "boot",
  user: null,

  restore: async () => {
    const saved = globalThis.localStorage?.getItem(TOKEN_KEY) ?? null;
    if (!saved) {
      set({ status: "locked" });
      return;
    }
    setAuthToken(saved);
    const { data } = await api.GET("/api/v1/auth/session").catch(() => ({ data: undefined }));
    if (data) {
      set({
        status: "open",
        user: { username: data.username, uid: data.uid, groups: data.groups ?? [] },
      });
    } else {
      setAuthToken(null);
      globalThis.localStorage?.removeItem(TOKEN_KEY);
      set({ status: "locked" });
    }
  },

  complete: (token, user) => {
    setAuthToken(token);
    globalThis.localStorage?.setItem(TOKEN_KEY, token);
    set({ status: "open", user });
  },

  lock: async () => {
    await api.POST("/api/v1/auth/logout").catch(() => undefined);
    setAuthToken(null);
    globalThis.localStorage?.removeItem(TOKEN_KEY);
    set({ status: "locked", user: null });
  },
}));
