import { describe, expect, it } from "vitest";
import { highlight, MAX_HIGHLIGHT_TOKENS } from "./highlight";
import { MAX_TEXT_BYTES, previewKind } from "./kind";

describe("预览格式与安全文本", () => {
  it("HTML和SVG只显示文本，未知类型不嵌入", () => {
    expect(previewKind("a.HTML")).toBe("text");
    expect(previewKind("a.svg")).toBe("text");
    expect(previewKind("a.bin")).toBe("unsupported");
    expect(MAX_TEXT_BYTES).toBe(655360);
  });
  it.each(["ts", "py", "rs"])("%s按需着色且逐字保留原文", async (ext) => {
    const source = 'const value = "<script>alert(1)</script>"; // 注释\nreturn 123';
    const tokens = await highlight(`a.${ext}`, source);
    expect(tokens?.map((token) => token.text).join("")).toBe(source);
    expect(tokens?.some((token) => token.kind === "keyword")).toBe(true);
    expect(tokens?.some((token) => token.kind === "string")).toBe(true);
  });
  it("Unicode、组合字符、CRLF和恶意HTML逐字保持", async () => {
    const source =
      'const 名字 = "你好👩🏽‍💻e\u0301𠮷\u202e";\r\n// <img src=x onerror=alert(1)>\nconst html = "<script>alert(1)</script>";\t';
    const tokens = await highlight("unicode.ts", source);
    expect(tokens?.map((token) => token.text).join("")).toBe(source);
    for (const token of tokens ?? [])
      expect(source.slice(token.offset, token.offset + token.text.length)).toBe(token.text);
  });
  it("token阈值边界：20000以内着色，超过阈值回退", async () => {
    expect(MAX_HIGHLIGHT_TOKENS).toBe(20_000);
    expect((await highlight("a.ts", "x ".repeat(10_000)))?.length).toBe(20_000);
    expect(await highlight("a.ts", "x ".repeat(10_001))).toBeNull();
  });
  it("不支持的语言及密集token回退纯文本", async () => {
    expect(await highlight("a.txt", "plain")).toBeNull();
    expect(await highlight("a.ts", "const x = 1;\n".repeat(20_000))).toBeNull();
  });
});
