import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { setAuthToken } from "@/api/client";
import { resetSessionResources } from "@/session/lifecycle";
import { AccessLease, RENEW_MS } from "./access";

const response = (id = "one") => ({
  id,
  url: `/api/v1/file-access/${id}/content`,
  expires_in_secs: 600,
});
beforeEach(() => {
  vi.useFakeTimers();
  setAuthToken("alice");
});
afterEach(() => {
  resetSessionResources();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("文件访问生命周期", () => {
  it("Bearer JSON 创建，5min续期；释放停止定时器", async () => {
    const fetcher = vi.fn().mockImplementation(() => Promise.resolve(Response.json(response())));
    vi.stubGlobal("fetch", fetcher);
    const lease = new AccessLease();
    await lease.open("/private/movie.mp4", "preview");
    expect(fetcher).toHaveBeenCalledWith(
      "/api/v1/file-access",
      expect.objectContaining({
        method: "POST",
        credentials: "same-origin",
        headers: { Authorization: "Bearer alice", "Content-Type": "application/json" },
        body: JSON.stringify({ path: "/private/movie.mp4", purpose: "preview" }),
      }),
    );
    const failed = vi.fn();
    lease.renew(failed);
    await vi.advanceTimersByTimeAsync(RENEW_MS);
    expect(fetcher).toHaveBeenCalledWith(
      "/api/v1/file-access/one/renew",
      expect.objectContaining({ method: "POST" }),
    );
    lease.dispose();
    await vi.advanceTimersByTimeAsync(RENEW_MS * 2);
    expect(fetcher).toHaveBeenCalledTimes(3);
    expect(failed).not.toHaveBeenCalled();
  });
  it("换账号使用旧凭据释放记录，同时中止未完成请求", async () => {
    const fetcher = vi.fn().mockImplementation(() => Promise.resolve(Response.json(response())));
    vi.stubGlobal("fetch", fetcher);
    const lease = new AccessLease();
    await lease.open("/a", "preview");
    const signal = fetcher.mock.calls[0]![1].signal as AbortSignal;
    setAuthToken("bob");
    expect(signal.aborted).toBe(true);
    expect(fetcher).toHaveBeenLastCalledWith(
      "/api/v1/file-access/one",
      expect.objectContaining({ method: "DELETE", headers: { Authorization: "Bearer alice" } }),
    );
  });
  it("创建响应迟到仍释放，不能复活已关闭预览", async () => {
    let resolve!: (r: Response) => void;
    const fetcher = vi
      .fn()
      .mockImplementationOnce(
        () =>
          new Promise<Response>((r) => {
            resolve = r;
          }),
      )
      .mockResolvedValue(new Response(null, { status: 204 }));
    vi.stubGlobal("fetch", fetcher);
    const lease = new AccessLease();
    const pending = lease.open("/a", "preview");
    await Promise.resolve();
    lease.dispose();
    resolve(Response.json(response()));
    await expect(pending).rejects.toMatchObject({ name: "AbortError" });
    expect(fetcher).toHaveBeenLastCalledWith(
      "/api/v1/file-access/one",
      expect.objectContaining({ method: "DELETE" }),
    );
  });
  it("续期失败释放，并通知UI卸载媒体", async () => {
    const fetcher = vi
      .fn()
      .mockResolvedValueOnce(Response.json(response()))
      .mockResolvedValueOnce(Response.json({ message: "访问已过期" }, { status: 401 }))
      .mockResolvedValue(new Response(null, { status: 204 }));
    vi.stubGlobal("fetch", fetcher);
    const lease = new AccessLease();
    await lease.open("/a", "preview");
    const failed = vi.fn();
    lease.renew(failed);
    await vi.advanceTimersByTimeAsync(RENEW_MS);
    expect(failed).toHaveBeenCalledWith(new Error("访问已过期"));
    expect(fetcher).toHaveBeenLastCalledWith(
      "/api/v1/file-access/one",
      expect.objectContaining({ method: "DELETE" }),
    );
  });
  it("预览释放不释放另一条下载，下载过期才回收", async () => {
    const fetcher = vi
      .fn()
      .mockResolvedValueOnce(Response.json(response("preview")))
      .mockResolvedValueOnce(Response.json(response("download")))
      .mockResolvedValue(new Response(null, { status: 204 }));
    vi.stubGlobal("fetch", fetcher);
    const preview = new AccessLease();
    const download = new AccessLease();
    await preview.open("/a", "preview");
    await download.open("/a", "download");
    download.expireAfter(600);
    preview.dispose();
    expect(
      fetcher.mock.calls.filter((call) => call[1].method === "DELETE").map((call) => call[0]),
    ).toEqual(["/api/v1/file-access/preview"]);
    await vi.advanceTimersByTimeAsync(600_000);
    expect(fetcher).toHaveBeenLastCalledWith(
      "/api/v1/file-access/download",
      expect.objectContaining({ method: "DELETE" }),
    );
  });
  it("拒绝外源和可执行URL", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(Response.json({ ...response(), url: "javascript:alert(1)" })),
    );
    const lease = new AccessLease();
    await expect(lease.open("/a", "preview")).rejects.toThrow("响应无效");
    lease.dispose();
  });
});
