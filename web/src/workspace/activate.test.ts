import { describe, expect, it } from "vitest";
import { activateEntry, resolveLinkTarget } from "./activate";

const HOME = "C:\\Users\\k";

const dir = (name: string) => ({ kind: "dir", name });
const file = (name: string) => ({ kind: "file", name });
const link = (name: string, target?: string, target_kind?: string) => ({
  kind: "symlink",
  name,
  target,
  target_kind,
});

describe("resolveLinkTarget unix", () => {
  it("绝对目标原样用", () =>
    expect(resolveLinkTarget("/home/k", "/etc/hosts", "unix")).toBe("/etc/hosts"));
  it("相对目标接到当前目录后面", () =>
    expect(resolveLinkTarget("/home/k", "notes", "unix")).toBe("/home/k/notes"));
  it("带 .. 的相对目标交给服务端规范化，这里只拼", () =>
    expect(resolveLinkTarget("/home/k", "../other", "unix")).toBe("/home/k/../other"));
});

describe("resolveLinkTarget windows", () => {
  it("盘符开头是绝对路径", () =>
    expect(resolveLinkTarget(HOME, "C:\\Windows", "windows")).toBe("C:\\Windows"));
  it("UNC 路径是绝对路径", () =>
    expect(resolveLinkTarget(HOME, "\\\\nas\\share", "windows")).toBe("\\\\nas\\share"));
  it("相对目标接到当前目录后面", () =>
    expect(resolveLinkTarget(HOME, "Documents", "windows")).toBe("C:\\Users\\k\\Documents"));
});

describe("activateEntry", () => {
  it("目录：进去", () =>
    expect(activateEntry("/home/k", dir("src"), "unix")).toEqual({
      kind: "enter",
      path: "/home/k/src",
    }));

  it("指向目录的 junction：直接进到目标", () =>
    expect(
      activateEntry(HOME, link("Recent", "C:\\Users\\k\\AppData\\Recent", "dir"), "windows"),
    ).toEqual({ kind: "enter", path: "C:\\Users\\k\\AppData\\Recent" }));

  it("指向文件的链接：跳到它所在的目录并选中它", () =>
    expect(activateEntry("/home/k", link("cfg", "/etc/app/app.conf", "file"), "unix")).toEqual({
      kind: "reveal",
      dir: "/etc/app",
      name: "app.conf",
    }));

  it("windows 上指向文件的链接也按反斜杠切", () =>
    expect(activateEntry(HOME, link("cfg", "C:\\ProgramData\\app.ini", "file"), "windows")).toEqual(
      { kind: "reveal", dir: "C:\\ProgramData", name: "app.ini" },
    ));

  it("目标类型未知（断链、老服务端）：仍然按目录去试，让服务端报错", () =>
    expect(activateEntry("/home/k", link("gone", "/nowhere"), "unix")).toEqual({
      kind: "enter",
      path: "/nowhere",
    }));

  it("没有目标的链接：无处可去", () =>
    expect(activateEntry("/home/k", link("weird"), "unix")).toBeNull());

  it("普通文件：暂时无处可去（预览界面还没有）", () =>
    expect(activateEntry("/home/k", file("a.txt"), "unix")).toBeNull());

  it("当前目录未知时不动", () => expect(activateEntry(null, dir("src"), "unix")).toBeNull());
});
