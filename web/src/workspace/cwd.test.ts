import { describe, expect, it } from "vitest";
import { buildCdBytes, isPowerShell, parseOsc7, pollable } from "./cwd";

const dec = (b: Uint8Array) => new TextDecoder().decode(b);

describe("parseOsc7", () => {
  it("常规 unix 路径", () =>
    expect(parseOsc7("file://myhost/home/kylin/proj")).toBe("/home/kylin/proj"));
  it("空 host", () => expect(parseOsc7("file:///etc")).toBe("/etc"));
  it("百分号编码解码", () => expect(parseOsc7("file://h/home/a%20b")).toBe("/home/a b"));
  it("未编码的空格原样通过（bash 注入就是裸 $PWD）", () =>
    expect(parseOsc7("file://h/home/a b")).toBe("/home/a b"));
  it("windows 形状翻转为反斜杠", () =>
    expect(parseOsc7("file://h/C:/Users/kylin")).toBe("C:\\Users\\kylin"));
  it("不是 file:// 一律拒绝", () => {
    expect(parseOsc7("http://x/etc")).toBeNull();
    expect(parseOsc7("gibberish")).toBeNull();
    expect(parseOsc7("file://hostonly")).toBeNull();
  });
});

describe("shell 判定", () => {
  it("powershell 不可轮询——Set-Location 不改进程 cwd，轮询给的是静默旧值", () => {
    expect(isPowerShell("C:\\WINDOWS\\System32\\WindowsPowerShell\\v1.0\\powershell.exe")).toBe(
      true,
    );
    expect(isPowerShell("/usr/local/bin/pwsh")).toBe(true);
    expect(pollable("C:\\Program Files\\PowerShell\\7\\pwsh.exe")).toBe(false);
  });
  it("其余 shell 可轮询", () => {
    expect(pollable("/bin/zsh")).toBe(true);
    expect(pollable("C:\\Windows\\System32\\cmd.exe")).toBe(true);
    expect(pollable(undefined)).toBe(true);
  });
});

describe("buildCdBytes", () => {
  it("posix 单引号与逃逸", () =>
    expect(dec(buildCdBytes("/tmp/it's", "/bin/zsh"))).toBe("cd '/tmp/it'\\''s'\n"));
  it("cmd 用双引号加 /d", () =>
    expect(dec(buildCdBytes("D:\\数据", "C:\\Windows\\System32\\cmd.exe"))).toBe(
      'cd /d "D:\\数据"\r\n',
    ));
  it("powershell 单引号翻倍", () =>
    expect(dec(buildCdBytes("C:\\it's", "pwsh.exe"))).toBe("cd 'C:\\it''s'\r\n"));
});
