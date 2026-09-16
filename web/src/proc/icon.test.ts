import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { setAuthToken } from "@/api/client";
import { ensureIcon, iconUrl, isIconName, peekIcon } from "./icon";

/** 合法 PNG 的最小替身：只有文件头，取回层只校验到这里。 */
const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00]);

function stubFetch(reply: () => Response) {
  const spy = vi.fn(() => Promise.resolve(reply()));
  vi.stubGlobal("fetch", spy);
  return spy;
}

describe("isIconName", () => {
  it("放行普通映像名", () => {
    expect(isIconName("chrome.exe")).toBe(true);
    expect(isIconName("Code - Insiders.exe")).toBe(true);
  });

  it("挡住路径分隔符、`..` 与超长名（服务端会回 400，不必发这一趟）", () => {
    expect(isIconName("")).toBe(false);
    expect(isIconName("a/b.exe")).toBe(false);
    expect(isIconName("a\\b.exe")).toBe(false);
    expect(isIconName("..\\..\\windows\\system32\\config\\sam")).toBe(false);
    expect(isIconName("x..y.exe")).toBe(false);
    expect(isIconName(`${"a".repeat(256)}.exe`)).toBe(false);
  });
});

describe("iconUrl", () => {
  it("名字进路径段前要转义", () => {
    expect(iconUrl("chrome.exe")).toBe("/api/v1/processes/icon/chrome.exe");
    expect(iconUrl("My App.exe")).toBe("/api/v1/processes/icon/My%20App.exe");
  });
});

describe("ensureIcon", () => {
  beforeEach(() => {
    setAuthToken("t0ken");
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
    setAuthToken(null);
  });

  it("取到 PNG 就缓存成 data URL", async () => {
    stubFetch(() => new Response(PNG, { status: 200 }));
    await ensureIcon("ok.exe");
    expect(peekIcon("ok.exe")).toMatch(/^data:image\/png;base64,/);
  });

  it("404 记负缓存：同一个名字不再重复请求", async () => {
    const spy = stubFetch(() => new Response(null, { status: 404 }));
    await ensureIcon("missing.exe");
    await ensureIcon("missing.exe");
    expect(peekIcon("missing.exe")).toBeNull();
    expect(spy).toHaveBeenCalledTimes(1);
  });

  it("同名并发只发一次请求", async () => {
    const spy = stubFetch(() => new Response(PNG, { status: 200 }));
    await Promise.all([ensureIcon("dup.exe"), ensureIcon("dup.exe"), ensureIcon("dup.exe")]);
    expect(spy).toHaveBeenCalledTimes(1);
  });

  it("响应体不是 PNG 就当作没有图标，不交给 <img> 去解码失败", async () => {
    stubFetch(() => new Response(new Uint8Array([0x3c, 0x21, 0x64]), { status: 200 }));
    await ensureIcon("html.exe");
    expect(peekIcon("html.exe")).toBeNull();
  });

  it("401 不写负缓存：会话恢复后应当能重新取到", async () => {
    const spy = stubFetch(() => new Response(null, { status: 401 }));
    await ensureIcon("later.exe");
    await ensureIcon("later.exe");
    expect(spy).toHaveBeenCalledTimes(2);
  });

  it("没有会话就不发请求", async () => {
    setAuthToken(null);
    const spy = stubFetch(() => new Response(PNG, { status: 200 }));
    await ensureIcon("anon.exe");
    expect(spy).not.toHaveBeenCalled();
  });

  it("不合法的名字直接落负缓存，一趟都不发", async () => {
    const spy = stubFetch(() => new Response(PNG, { status: 200 }));
    await ensureIcon("../../etc/passwd");
    expect(spy).not.toHaveBeenCalled();
    expect(peekIcon("../../etc/passwd")).toBeNull();
  });

  it("网络异常不抛，也不写负缓存", async () => {
    const spy = vi.fn(() => Promise.reject(new Error("offline")));
    vi.stubGlobal("fetch", spy);
    await expect(ensureIcon("offline.exe")).resolves.toBeUndefined();
    expect(peekIcon("offline.exe")).toBeNull();
    await ensureIcon("offline.exe");
    expect(spy).toHaveBeenCalledTimes(2);
  });

  it("TTL 过期后重新取", async () => {
    vi.useFakeTimers();
    const spy = stubFetch(() => new Response(PNG, { status: 200 }));
    await ensureIcon("ttl.exe");
    await ensureIcon("ttl.exe");
    expect(spy).toHaveBeenCalledTimes(1);
    vi.advanceTimersByTime(5 * 60 * 1000 + 1);
    await ensureIcon("ttl.exe");
    expect(spy).toHaveBeenCalledTimes(2);
  });
});
