import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api, authHeaders } from "@/api/client";
import { queryClient } from "@/api/queryClient";
import { useWorkspace } from "@/workspace/store";
import { fetchThumb } from "@/workspace/thumbs";
import { sessionSignal } from "./lifecycle";
import { useSession } from "./useSession";

vi.mock("@/lib/ws", () => ({ wsClient: { connect: vi.fn(), close: vi.fn() } }));
vi.mock("@/metrics/live", () => ({ startLive: vi.fn(), stopLive: vi.fn() }));
const alice = { username: "alice", uid: 1001, groups: [] };
const bob = { username: "bob", uid: 1002, groups: [] };
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

beforeEach(() => {
  const values = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => values.set(key, value),
    removeItem: (key: string) => values.delete(key),
  });
  vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response(null, { status: 204 }));
});
afterEach(async () => {
  await useSession.getState().lock();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("身份切换与在途响应", () => {
  it("锁定立即关门，捕获旧 token；迟到的注销不能清掉新会话", async () => {
    const logout = deferred<Response>();
    vi.mocked(fetch).mockReturnValueOnce(logout.promise);
    useSession.getState().complete("alice-token", alice);
    const oldScope = sessionSignal();
    const locking = useSession.getState().lock();
    expect(useSession.getState().status).toBe("locked");
    expect(authHeaders()).toBeNull();
    expect(oldScope.aborted).toBe(true);
    expect(fetch).toHaveBeenCalledWith(
      "/api/v1/auth/logout",
      expect.objectContaining({
        headers: { Authorization: "Bearer alice-token" },
        signal: expect.any(AbortSignal),
      }),
    );
    useSession.getState().complete("bob-token", bob);
    logout.resolve(new Response(null, { status: 204 }));
    await locking;
    expect(useSession.getState().user).toEqual(bob);
    expect(authHeaders()).toEqual({ Authorization: "Bearer bob-token" });
  });

  it("换身份清工作区、查询、Blob；下一个用户必须重新取同一路径", async () => {
    vi.mocked(fetch).mockImplementation(
      async (url) =>
        new Response(String(url).includes("thumb") ? new Blob(["private-image"]) : null),
    );
    const revoke = vi.spyOn(URL, "revokeObjectURL");
    useSession.getState().complete("alice-token", alice);
    useWorkspace.getState().setCwd("/home/alice/private");
    useWorkspace
      .getState()
      .addTab({ id: "alice-terminal", title: "private shell", status: "live" });
    queryClient.setQueryData(["dir", "/shared"], { private: true });
    const first = await fetchThumb("/shared/private.jpg");
    expect(first).not.toBeNull();
    await useSession.getState().lock();
    useSession.getState().complete("bob-token", bob);
    expect(useWorkspace.getState().cwd).toBeNull();
    expect(useWorkspace.getState().tabs).toEqual([]);
    expect(queryClient.getQueryData(["dir", "/shared"])).toBeUndefined();
    expect(revoke).toHaveBeenCalledWith(first?.url);
    const second = await fetchThumb("/shared/private.jpg");
    expect(second?.url).not.toBe(first?.url);
    expect(
      vi.mocked(fetch).mock.calls.filter(([url]) => String(url).includes("thumb")),
    ).toHaveLength(2);
  });

  it("已收到 headers 的旧图片，body 迟到也不能污染新缓存或移除新请求", async () => {
    const body = deferred<Blob>();
    const oldResponse = new Response();
    vi.spyOn(oldResponse, "blob").mockReturnValue(body.promise);
    vi.mocked(fetch).mockResolvedValueOnce(oldResponse);
    useSession.getState().complete("alice-token", alice);
    const old = fetchThumb("/shared/x.jpg");
    await vi.waitFor(() => expect(oldResponse.blob).toHaveBeenCalled());
    useSession.getState().complete("bob-token", bob);
    const nextResponse = deferred<Response>();
    vi.mocked(fetch).mockReturnValueOnce(nextResponse.promise);
    const next = fetchThumb("/shared/x.jpg");
    body.resolve(new Blob(["old"]));
    expect(await old).toBeNull();
    expect(fetchThumb("/shared/x.jpg")).toBe(next);
    nextResponse.resolve(new Response(new Blob(["new"])));
    const current = await next;
    expect(await fetchThumb("/shared/x.jpg")).toBe(current);
  });

  it("旧 restore 迟到不能覆盖完成的新登录", async () => {
    const pending = deferred<Awaited<ReturnType<typeof api.GET>>>();
    vi.spyOn(api, "GET").mockReturnValueOnce(pending.promise as never);
    // 使用生产 token key，避免测试与存储命名脱节。
    const { SESSION_TOKEN_KEY } = await import("./token");
    localStorage.setItem(SESSION_TOKEN_KEY, "old-token");
    const restoring = useSession.getState().restore();
    useSession.getState().complete("bob-token", bob);
    pending.resolve({ data: { username: "alice", uid: 1001 }, response: new Response() } as never);
    await restoring;
    expect(useSession.getState().user).toEqual(bob);
  });
  it("实际 API 中间件传递身份，并在锁定时中断进行中的请求", async () => {
    useSession.getState().complete("alice-token", alice);
    const reached = deferred<Request>();
    const request = api.GET("/api/v1/auth/session", {
      baseUrl: "http://localhost",
      fetch: (req) =>
        new Promise<Response>((_resolve, reject) => {
          reached.resolve(req);
          req.signal.addEventListener("abort", () => reject(req.signal.reason), { once: true });
        }),
    });
    const seen = await reached.promise;
    expect(seen.headers.get("Authorization")).toBe("Bearer alice-token");
    const rejected = expect(request).rejects.toMatchObject({ name: "AbortError" });
    await useSession.getState().lock();
    await rejected;
    expect(seen.signal.aborted).toBe(true);
  });
});
