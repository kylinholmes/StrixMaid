import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { setAuthToken } from "@/api/client";
import { extOf, fetchTypeIcon, resetForTest } from "./sysicons";

describe("extOf", () => {
  it("常规扩展名取出并小写", () => {
    expect(extOf("报告.PDF")).toBe("pdf");
    expect(extOf("a.tar.gz")).toBe("gz");
    expect(extOf("main.rs")).toBe("rs");
  });

  it("取不出的形态一律 null", () => {
    expect(extOf("Makefile")).toBe(null); // 无点
    expect(extOf(".bashrc")).toBe(null); // dotfile：点在开头不是扩展名
    expect(extOf("name.")).toBe(null); // 尾点
    expect(extOf("a.b c")).toBe(null); // 空白（后端会 400，不该发请求）
    expect(extOf(`x.${"a".repeat(33)}`)).toBe(null); // 超长
  });
});

describe("fetchTypeIcon", () => {
  const fetchMock = vi.fn<typeof fetch>();

  beforeEach(() => {
    resetForTest();
    setAuthToken("test-token");
    fetchMock.mockReset();
    vi.stubGlobal("fetch", fetchMock);
    // node 测试环境没有 createObjectURL。
    vi.stubGlobal("URL", {
      ...URL,
      createObjectURL: vi.fn(() => `blob:${Math.random()}`),
    });
  });

  afterEach(() => {
    setAuthToken(null);
    vi.unstubAllGlobals();
  });

  const png = () => new Response(new Blob([new Uint8Array([0x89, 0x50])]), { status: 200 });

  it("探测 404 后整个会话不再发请求", async () => {
    fetchMock.mockResolvedValue(new Response(null, { status: 404 }));
    expect(await fetchTypeIcon("pdf")).toBe(null);
    expect(await fetchTypeIcon("docx")).toBe(null);
    expect(await fetchTypeIcon("rs")).toBe(null);
    // 只有那一次 txt 探测。
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(String(fetchMock.mock.calls[0]?.[0])).toContain("/files/icon/txt");
  });

  it("探测成功后按扩展名取并缓存", async () => {
    fetchMock.mockImplementation(() => Promise.resolve(png()));
    const a = await fetchTypeIcon("pdf");
    expect(a).not.toBe(null);
    const b = await fetchTypeIcon("pdf");
    expect(b).toBe(a); // 命中缓存，同一个 object URL
    // 探测（txt）一次 + pdf 一次。
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it("探测那张 txt 自己也进缓存", async () => {
    fetchMock.mockImplementation(() => Promise.resolve(png()));
    await fetchTypeIcon("md");
    await fetchTypeIcon("txt");
    // txt 不再另发：探测时已经存好。
    expect(
      fetchMock.mock.calls.filter(([u]) => String(u).includes("/files/icon/txt")),
    ).toHaveLength(1);
  });

  it("没登录时不发请求也不定论", async () => {
    setAuthToken(null);
    expect(await fetchTypeIcon("pdf")).toBe(null);
    expect(fetchMock).not.toHaveBeenCalled();
    // 登录后同一个会话还能问成。
    setAuthToken("test-token");
    fetchMock.mockImplementation(() => Promise.resolve(png()));
    expect(await fetchTypeIcon("pdf")).not.toBe(null);
  });

  it("网络错误不落负缓存", async () => {
    fetchMock.mockRejectedValueOnce(new Error("net down"));
    expect(await fetchTypeIcon("pdf")).toBe(null);
    fetchMock.mockImplementation(() => Promise.resolve(png()));
    expect(await fetchTypeIcon("pdf")).not.toBe(null);
  });
});
