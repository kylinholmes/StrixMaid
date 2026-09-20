import { describe, expect, it } from "vitest";
import { isDriveRoot, isVirtualRoot, joinPath, parentPath, platformOf } from "./path";

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
