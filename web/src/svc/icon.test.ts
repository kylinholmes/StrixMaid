import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { setAuthToken } from "@/api/client";
import {
  clearSvcIcons,
  ensureSvcIcon,
  genericIconUrl,
  isSvcIconName,
  peekSvcIcon,
  svcIconUrl,
} from "./icon";

/** 合法 PNG 的最小替身：只有文件头，取回层只校验到这里。 */
const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00]);

/** 按 URL 分派的假后端：没列出的路径一律 404。 */
function stubFetch(routes: Record<string, () => Response>) {
  const spy = vi.fn((url: string) => {
    const reply = routes[url];
    return Promise.resolve(reply === undefined ? new Response(null, { status: 404 }) : reply());
  });
  vi.stubGlobal("fetch", spy);
  return spy;
}

const GENERIC = "/api/v1/services/icon-generic";

describe("isSvcIconName", () => {
  it("放行真实世界里的服务名（空格、括号、逗号、后缀）", () => {
    expect(isSvcIconName("Spooler")).toBe(true);
    expect(isSvcIconName("Spooler.service")).toBe(true);
    expect(isSvcIconName("Net Driver HPZ12")).toBe(true);
    expect(isSvcIconName("Intel(R) TPM Provisioning Service")).toBe(true);
    expect(isSvcIconName("SangforDnsDrv_7,6,9,1")).toBe(true);
  });

  it("挡住路径分隔符、`..` 与超长名（服务端会回 400，不必发这一趟）", () => {
    expect(isSvcIconName("")).toBe(false);
    // 反斜杠会被拼进注册表键路径，这道闸是承重的
    expect(isSvcIconName("Spooler\\Parameters")).toBe(false);
    expect(isSvcIconName("a/b")).toBe(false);
    expect(isSvcIconName("..")).toBe(false);
    expect(isSvcIconName(`${"a".repeat(265)}`)).toBe(false);
  });
});

describe("URL 形状", () => {
  it("名字进路径段前要转义", () => {
    expect(svcIconUrl("Spooler.service")).toBe("/api/v1/services/icon/Spooler.service");
    expect(svcIconUrl("Net Driver HPZ12")).toBe("/api/v1/services/icon/Net%20Driver%20HPZ12");
  });

  it("通用图标端点不带参数", () => {
    expect(genericIconUrl()).toBe(GENERIC);
  });
});

describe("ensureSvcIcon", () => {
  beforeEach(() => {
    setAuthToken("t0ken");
    clearSvcIcons();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
    setAuthToken(null);
    clearSvcIcons();
  });

  it("有自己的图标时用自己的，不去碰通用端点", async () => {
    const spy = stubFetch({
      "/api/v1/services/icon/Everything": () => new Response(PNG, { status: 200 }),
    });
    await ensureSvcIcon("Everything");
    expect(peekSvcIcon("Everything")).toMatch(/^data:image\/png;base64,/);
    expect(spy).toHaveBeenCalledTimes(1);
    expect(spy).not.toHaveBeenCalledWith(GENERIC, expect.anything());
  });

  it("按名字 404 时回落到通用齿轮", async () => {
    stubFetch({ [GENERIC]: () => new Response(PNG, { status: 200 }) });
    await ensureSvcIcon("Appinfo");
    expect(peekSvcIcon("Appinfo")).toMatch(/^data:image\/png;base64,/);
  });

  it("通用齿轮对整张表只取一次", async () => {
    const spy = stubFetch({ [GENERIC]: () => new Response(PNG, { status: 200 }) });
    const svchost = ["AarSvc", "AJRouter", "AppIDSvc", "Appinfo", "AppMgmt"];
    await Promise.all(svchost.map(ensureSvcIcon));
    const generic = spy.mock.calls.filter(([url]) => url === GENERIC);
    expect(generic).toHaveLength(1);
    // 每个服务各自那一趟仍然要发（它们的名字不同，结论也可能不同）
    expect(spy).toHaveBeenCalledTimes(svchost.length + 1);
    for (const name of svchost) {
      expect(peekSvcIcon(name)).toMatch(/^data:image\/png;base64,/);
    }
  });

  it("通用端点也 404（非 Windows）时留空，不画齿轮", async () => {
    stubFetch({});
    await ensureSvcIcon("sshd.service");
    expect(peekSvcIcon("sshd.service")).toBeNull();
  });

  it("还没有结论之前给空白，避免真图标到达时闪一下齿轮", async () => {
    stubFetch({
      "/api/v1/services/icon/Everything": () => new Response(PNG, { status: 200 }),
      [GENERIC]: () => new Response(PNG, { status: 200 }),
    });
    // 先让通用那张进缓存
    await ensureSvcIcon("Appinfo");
    expect(peekSvcIcon("Appinfo")).not.toBeNull();
    // 一个还没取过的名字：即便通用那张已经在手，也不能先拿它顶上
    expect(peekSvcIcon("Everything")).toBeNull();
    await ensureSvcIcon("Everything");
    expect(peekSvcIcon("Everything")).not.toBeNull();
  });

  it("同名并发只发一次请求", async () => {
    const spy = stubFetch({
      "/api/v1/services/icon/dup": () => new Response(PNG, { status: 200 }),
    });
    await Promise.all([ensureSvcIcon("dup"), ensureSvcIcon("dup"), ensureSvcIcon("dup")]);
    expect(spy).toHaveBeenCalledTimes(1);
  });

  it("响应体不是 PNG 就当作没有图标，不交给 <img> 去解码失败", async () => {
    stubFetch({
      "/api/v1/services/icon/html": () =>
        new Response(new Uint8Array([0x3c, 0x21, 0x64]), {
          status: 200,
        }),
    });
    await ensureSvcIcon("html");
    expect(peekSvcIcon("html")).toBeNull();
  });

  it("401 不写负缓存：会话恢复后应当能重新取到", async () => {
    const spy = stubFetch({
      "/api/v1/services/icon/later": () => new Response(null, { status: 401 }),
      [GENERIC]: () => new Response(null, { status: 401 }),
    });
    await ensureSvcIcon("later");
    await ensureSvcIcon("later");
    expect(spy.mock.calls.filter(([url]) => url.endsWith("/later"))).toHaveLength(2);
  });

  it("没有会话就不发请求", async () => {
    setAuthToken(null);
    const spy = stubFetch({});
    await ensureSvcIcon("anon");
    expect(spy).not.toHaveBeenCalled();
  });

  it("不合法的名字直接落负缓存，一趟都不发", async () => {
    const spy = stubFetch({ [GENERIC]: () => new Response(PNG, { status: 200 }) });
    await ensureSvcIcon("..\\..\\Services\\Other");
    // 名字本身那一趟不发；通用那张仍然会取（这一行总要显示点什么）
    expect(spy.mock.calls.every(([url]) => url === GENERIC)).toBe(true);
  });

  it("网络异常不抛，也不写负缓存", async () => {
    const spy = vi.fn((_url: string) => Promise.reject(new Error("offline")));
    vi.stubGlobal("fetch", spy);
    await expect(ensureSvcIcon("offline")).resolves.toBeUndefined();
    expect(peekSvcIcon("offline")).toBeNull();
    await ensureSvcIcon("offline");
    expect(spy.mock.calls.filter(([url]) => url.endsWith("/offline"))).toHaveLength(2);
  });

  it("TTL 过期后重新取", async () => {
    vi.useFakeTimers();
    const spy = stubFetch({
      "/api/v1/services/icon/ttl": () => new Response(PNG, { status: 200 }),
    });
    await ensureSvcIcon("ttl");
    await ensureSvcIcon("ttl");
    expect(spy).toHaveBeenCalledTimes(1);
    vi.advanceTimersByTime(5 * 60 * 1000 + 1);
    await ensureSvcIcon("ttl");
    expect(spy).toHaveBeenCalledTimes(2);
  });
});
