import { beforeEach, describe, expect, it, vi } from "vitest";
import { TermSocket } from "./termsocket";

/** 最小可控的 WebSocket 替身：测试直接拨弄 on* 回调来模拟服务端。 */
class MockWS {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  static last: MockWS | null = null;
  onopen: (() => void) | null = null;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  sent: unknown[] = [];
  binaryType = "";
  readyState = 1; // OPEN
  constructor(
    public url: string,
    public protocols?: string[],
  ) {
    MockWS.last = this;
  }
  send(d: unknown) {
    this.sent.push(d);
  }
  close() {
    this.readyState = 3;
    this.onclose?.();
  }
}

const noop = { onData() {}, onExit() {}, onStatus() {} };

describe("TermSocket", () => {
  beforeEach(() => {
    MockWS.last = null;
    vi.stubGlobal("WebSocket", MockWS as unknown as typeof WebSocket);
    // node 测试环境没有 location / localStorage，补最小替身。
    vi.stubGlobal("location", { protocol: "http:", host: "test.local" });
    const store = new Map<string, string>([["strixmaid.session.token", "tok"]]);
    vi.stubGlobal("localStorage", {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => void store.set(k, v),
    });
  });

  it("连接带 bearer 子协议与终端 id，且用 arraybuffer", () => {
    new TermSocket("abc", noop);
    expect(MockWS.last?.url).toContain("/ws/terminal/abc");
    expect(MockWS.last?.protocols).toEqual(["bearer", "tok"]);
    expect(MockWS.last?.binaryType).toBe("arraybuffer");
  });

  it("二进制帧交给 onData", () => {
    const seen: Uint8Array[] = [];
    new TermSocket("abc", { ...noop, onData: (b) => seen.push(b) });
    MockWS.last?.onmessage?.({ data: new TextEncoder().encode("hi").buffer });
    expect(seen).toHaveLength(1);
    expect(new TextDecoder().decode(seen[0])).toBe("hi");
  });

  it("exit 文本帧交给 onExit，之后的 onclose 不再当断线报告", () => {
    let exit: unknown = null;
    const status: boolean[] = [];
    new TermSocket("abc", {
      ...noop,
      onExit: (e) => {
        exit = e;
      },
      onStatus: (u) => status.push(u),
    });
    MockWS.last?.onmessage?.({ data: JSON.stringify({ t: "exit", reason: "exited", code: 42 }) });
    MockWS.last?.onclose?.();
    expect(exit).toEqual({ reason: "exited", code: 42 });
    // 退出后的连接关闭是尾声，不是断线——不能再发一个 onStatus(false)
    // 让界面把「已退出」改写成「连接断开可重连」。
    expect(status).not.toContain(false);
  });

  it("没收到 exit 就关闭 → 报断线", () => {
    const status: boolean[] = [];
    new TermSocket("abc", { ...noop, onStatus: (u) => status.push(u) });
    MockWS.last?.onclose?.();
    expect(status).toContain(false);
  });

  it("resize 发出 JSON 文本帧", () => {
    const ts = new TermSocket("abc", noop);
    ts.resize(120, 32);
    expect(MockWS.last?.sent).toContainEqual(JSON.stringify({ t: "resize", cols: 120, rows: 32 }));
  });

  it("send 在未打开时静默丢弃而不是抛错", () => {
    const ts = new TermSocket("abc", noop);
    if (MockWS.last) MockWS.last.readyState = 0; // CONNECTING
    expect(() => ts.send(new Uint8Array([1]))).not.toThrow();
    expect(MockWS.last?.sent).toHaveLength(0);
  });

  it("close 主动关闭后不报断线", () => {
    const status: boolean[] = [];
    const ts = new TermSocket("abc", { ...noop, onStatus: (u) => status.push(u) });
    ts.close();
    expect(status).not.toContain(false);
  });
});
