import { describe, expect, it } from "vitest";
import { fileIconUrl, folderIconUrl } from "./icons";

// 构建器可能把小 SVG 内联成 data: URI，URL 里不含 slug——断言映射**关系**
// 而不是 URL 子串：命中的与回落的必须能区分、同类的必须相同。

describe("folderIconUrl", () => {
  it("已知目录名命中专属图标（与通用文件夹可区分）", () => {
    const base = folderIconUrl("随便什么目录");
    for (const name of ["Downloads", ".git", "node_modules", "etc"]) {
      const u = folderIconUrl(name);
      expect(u).toBeTruthy();
      expect(u).not.toBe(base);
    }
    expect(folderIconUrl("Downloads")).toBe(folderIconUrl("download"));
  });
  it("认不出回落到普通文件夹，总有值且彼此一致", () => {
    expect(folderIconUrl("随便什么目录")).toBe(folderIconUrl("另一个未知目录"));
    expect(folderIconUrl("随便什么目录")).toBeTruthy();
  });
});

describe("fileIconUrl", () => {
  it("按扩展名命中，且不同语言可区分", () => {
    const rs = fileIconUrl("main.rs");
    const ts = fileIconUrl("app.TS");
    expect(rs).toBeTruthy();
    expect(ts).toBeTruthy();
    expect(rs).not.toBe(ts);
    expect(fileIconUrl("a.md")).toBe(fileIconUrl("b.markdown"));
    // CSV 与 Excel 分开选型（csv 是 Papirus、表格是 vscode-icons）。
    expect(fileIconUrl("data.csv")).not.toBe(fileIconUrl("data.xlsx"));
  });
  it("完整文件名优先于扩展名", () => {
    expect(fileIconUrl("Dockerfile")).toBe(fileIconUrl("docker-compose.yml"));
    expect(fileIconUrl("Makefile")).toBeTruthy();
    expect(fileIconUrl("nginx.conf")).not.toBe(fileIconUrl("app.conf"));
  });
  it("认不出返回 null——错的图标比没有更误导", () => {
    expect(fileIconUrl("mystery.xyz123")).toBeNull();
    expect(fileIconUrl("无扩展名")).toBeNull();
    // 以点开头且没有别的点：不是扩展名。
    expect(fileIconUrl(".bashrc")).toBeNull();
  });
});
