import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { setAuthToken } from "@/api/client";
import {
  COMPUTER_KEY,
  DIR_KEY,
  DOWNLOADS_KEY,
  DRIVE_KEY,
  entrySubject,
  extOf,
  fetchTypeIcon,
  GENERIC_FILE_KEY,
  HOME_KEY,
  iconKeysOf,
  resetForTest,
} from "./sysicons";

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

describe("iconKeysOf（主体→图标源的编译期穷尽判定）", () => {
  it("目录：unix 上带扩展名的是 bundle，按路径；普通目录只按类型", () => {
    expect(
      iconKeysOf({ kind: "dir", name: "Zed.app", fullPath: "/Applications/Zed.app" }, "unix"),
    ).toEqual({ pathKey: "/Applications/Zed.app", typeKey: DIR_KEY });
    expect(iconKeysOf({ kind: "dir", name: "docs", fullPath: "/home/k/docs" }, "unix")).toEqual({
      pathKey: null,
      typeKey: DIR_KEY,
    });
  });

  it("windows 平台永远没有 pathKey（后端按路径恒 404）", () => {
    for (const subject of [
      { kind: "dir", name: "Zed.app", fullPath: "C:\\Zed.app" },
      { kind: "symlink", name: "x", fullPath: "C:\\x" },
      { kind: "known-folder", name: "Desktop", fullPath: "C:\\Users\\k\\Desktop" },
      { kind: "home", fullPath: "C:\\Users\\k" },
      { kind: "mount", fullPath: "C:\\" },
    ] as const) {
      expect(iconKeysOf(subject, "windows").pathKey).toBe(null);
    }
  });

  it("文件按扩展名，无扩展名走 $file", () => {
    expect(iconKeysOf({ kind: "file", name: "a.PDF" }, "unix")).toEqual({
      pathKey: null,
      typeKey: "pdf",
    });
    expect(iconKeysOf({ kind: "file", name: "Makefile" }, "windows")).toEqual({
      pathKey: null,
      typeKey: GENERIC_FILE_KEY,
    });
  });

  it("unix 的左栏主体只按路径取（macOS 的真身图标就在那条路上）", () => {
    expect(iconKeysOf({ kind: "home", fullPath: "/x" }, "unix")).toEqual({
      pathKey: "/x",
      typeKey: null,
    });
    expect(iconKeysOf({ kind: "mount", fullPath: "/x" }, "unix")).toEqual({
      pathKey: "/x",
      typeKey: null,
    });
    expect(iconKeysOf({ kind: "known-folder", name: "Desktop", fullPath: "/x" }, "unix")).toEqual({
      pathKey: "/x",
      typeKey: null,
    });
  });

  it("windows 的左栏主体改用保留 key（原先一律回落内置图标集）", () => {
    const win = (subject: Parameters<typeof iconKeysOf>[0]) => iconKeysOf(subject, "windows");

    expect(win({ kind: "home", fullPath: "C:\\Users\\k" })).toEqual({
      pathKey: null,
      typeKey: HOME_KEY,
    });
    expect(win({ kind: "known-folder", name: "Downloads", fullPath: "C:\\x" })).toEqual({
      pathKey: null,
      typeKey: DOWNLOADS_KEY,
    });
    // 简体中文 Windows 的物理目录名就是中文，同样要认得。
    expect(win({ kind: "known-folder", name: "下载", fullPath: "C:\\x" }).typeKey).toBe(
      DOWNLOADS_KEY,
    );
    expect(win({ kind: "mount", fullPath: "C:\\" })).toEqual({
      pathKey: null,
      typeKey: DRIVE_KEY,
    });
    // 单个反斜杠是「全部驱动器」那个虚拟根，不是一块盘。
    expect(win({ kind: "mount", fullPath: "\\" }).typeKey).toBe(COMPUTER_KEY);
  });

  it("认不出的已知文件夹名不硬凑一个 key，交给内置图标回落", () => {
    expect(
      iconKeysOf({ kind: "known-folder", name: "Whatever", fullPath: "C:\\x" }, "windows").typeKey,
    ).toBe(null);
  });

  it("entrySubject 把设备/fifo/socket 都归为文件", () => {
    expect(entrySubject({ kind: "block_device", name: "sda" }, "/dev/sda")).toEqual({
      kind: "file",
      name: "sda",
    });
    expect(entrySubject({ kind: "dir", name: "d" }, "/d")).toEqual({
      kind: "dir",
      name: "d",
      fullPath: "/d",
    });
    expect(entrySubject({ kind: "symlink", name: "l" }, null)).toEqual({
      kind: "symlink",
      name: "l",
      fullPath: null,
    });
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
