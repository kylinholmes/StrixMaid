import { describe, expect, it } from "vitest";
import {
  guessHome,
  isDescendant,
  isDriveRoot,
  isVirtualRoot,
  joinPath,
  parentPath,
  platformOf,
  splitForCompletion,
} from "./path";

describe("platformOf", () => {
  it("识别 windows", () => expect(platformOf("windows")).toBe("windows"));
  it("其余当 unix", () => {
    expect(platformOf("ubuntu")).toBe("unix");
    expect(platformOf(undefined)).toBe("unix");
  });
});

describe("joinPath unix", () => {
  it("普通拼接", () => expect(joinPath("/etc", "hosts", "unix")).toBe("/etc/hosts"));
  it("根下拼接不出现双斜杠", () => expect(joinPath("/", "etc", "unix")).toBe("/etc"));
});

describe("joinPath windows", () => {
  it("普通目录用反斜杠", () =>
    expect(joinPath("C:\\Users", "kylin", "windows")).toBe("C:\\Users\\kylin"));
  it("虚拟根下的驱动器名就是完整路径，不拼接", () =>
    expect(joinPath("\\", "C:\\", "windows")).toBe("C:\\"));
  it("盘根下拼接", () => expect(joinPath("C:\\", "Windows", "windows")).toBe("C:\\Windows"));
});

describe("parentPath", () => {
  it("unix 根返回 null", () => expect(parentPath("/", "unix")).toBeNull());
  it("unix 普通", () => expect(parentPath("/etc/nginx", "unix")).toBe("/etc"));
  it("windows 盘根回到虚拟根", () => expect(parentPath("C:\\", "windows")).toBe("\\"));
  it("windows 虚拟根返回 null", () => expect(parentPath("\\", "windows")).toBeNull());
  it("windows 普通", () => expect(parentPath("C:\\Users\\kylin", "windows")).toBe("C:\\Users"));
});

describe("root 判定", () => {
  it("虚拟根", () => expect(isVirtualRoot("\\", "windows")).toBe(true));
  it("驱动器根", () => expect(isDriveRoot("C:\\", "windows")).toBe(true));
  it("普通名不是驱动器根", () => expect(isDriveRoot("Users", "windows")).toBe(false));
});

describe("guessHome", () => {
  it("windows", () => expect(guessHome("windows", "kylin", 1000)).toBe("C:\\Users\\kylin"));
  it("macos", () => expect(guessHome("macos", "kylin", 501)).toBe("/Users/kylin"));
  it("linux 普通用户", () => expect(guessHome("ubuntu", "alice", 1000)).toBe("/home/alice"));
  it("linux root", () => expect(guessHome("ubuntu", "root", 0)).toBe("/root"));
});

describe("splitForCompletion", () => {
  it("unix 常规", () =>
    expect(splitForCompletion("/home/ky", "unix")).toEqual({ parent: "/home", prefix: "ky" }));
  it("unix 根下", () =>
    expect(splitForCompletion("/ho", "unix")).toEqual({ parent: "/", prefix: "ho" }));
  it("以分隔符结尾 = 列整个目录", () =>
    expect(splitForCompletion("/home/", "unix")).toEqual({ parent: "/home", prefix: "" }));
  it("相对路径不补", () => expect(splitForCompletion("ho", "unix")).toBeNull());
  it("windows 盘根下", () =>
    expect(splitForCompletion("C:\\Us", "windows")).toEqual({ parent: "C:\\", prefix: "Us" }));
  it("windows 深层", () =>
    expect(splitForCompletion("C:\\Users\\ky", "windows")).toEqual({
      parent: "C:\\Users",
      prefix: "ky",
    }));
  it("windows 裸盘符不补", () => expect(splitForCompletion("C:", "windows")).toBeNull());
});

describe("isDescendant", () => {
  it("unix 直接与隔代子目录", () => {
    expect(isDescendant("/a/b", "/a", "unix")).toBe(true);
    expect(isDescendant("/a/b/c", "/a", "unix")).toBe(true);
    expect(isDescendant("/a", "/a", "unix")).toBe(false);
    expect(isDescendant("/ab", "/a", "unix")).toBe(false);
    expect(isDescendant("/a", "/a/b", "unix")).toBe(false);
    expect(isDescendant("/a/b", "/", "unix")).toBe(true);
  });
  it("windows 大小写不敏感 + 虚拟根", () => {
    expect(isDescendant("C:\\Users\\kylin", "c:\\users", "windows")).toBe(true);
    expect(isDescendant("C:\\Users", "C:\\", "windows")).toBe(true);
    expect(isDescendant("C:\\", "\\", "windows")).toBe(true);
    expect(isDescendant("D:\\x", "C:\\", "windows")).toBe(false);
  });
});
