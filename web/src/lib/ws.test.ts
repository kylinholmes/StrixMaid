import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { WsClient } from "./ws";

class Socket {
  static OPEN = 1;
  static instances: Socket[] = [];
  readyState = 0;
  onopen: (() => void) | null = null;
  onclose: ((event: { code: number; reason: string }) => void) | null = null;
  onmessage: ((event: { data: string }) => void) | null = null;
  send = vi.fn();
  close = vi.fn();
  constructor(
    readonly url: string,
    readonly protocols: string[],
  ) {
    Socket.instances.push(this);
  }
  opened() {
    this.readyState = Socket.OPEN;
    this.onopen?.();
  }
  ended() {
    this.readyState = 3;
    this.onclose?.({ code: 1000, reason: "" });
  }
  data(value: string) {
    this.onmessage?.({ data: JSON.stringify({ v: 1, t: "data", ch: "logs.follow", d: value }) });
  }
}

beforeEach(() => {
  vi.useFakeTimers();
  Socket.instances = [];
  vi.stubGlobal("WebSocket", Socket);
  vi.stubGlobal("location", { protocol: "http:", host: "localhost" });
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

it("换身份后旧 socket 的消息、open 和 close 不能污染新连接", () => {
  const client = new WsClient();
  const receive = vi.fn();
  const status = vi.fn();
  client.onStatus(status);
  client.subscribe("logs.follow", {}, receive);
  client.connect("alice");
  const old = Socket.instances[0]!;
  old.opened();
  client.connect("bob");
  const current = Socket.instances[1]!;
  current.opened();
  status.mockClear();
  old.data("private");
  old.opened();
  old.ended();
  expect(receive).not.toHaveBeenCalled();
  expect(status).not.toHaveBeenCalled();
  expect(old.close).toHaveBeenCalledOnce();
  expect(client.up).toBe(true);
  current.data("bob");
  expect(receive).toHaveBeenCalledExactlyOnceWith("bob");
  vi.advanceTimersByTime(20_000);
  expect(Socket.instances).toHaveLength(2);
  client.close();
  expect(vi.getTimerCount()).toBe(0);
});

it("主动锁定立即通知离线，迟到的 close 不再安排重连", () => {
  const client = new WsClient();
  const status = vi.fn();
  client.onStatus(status);
  client.connect("alice");
  const socket = Socket.instances[0]!;
  socket.opened();
  client.close();
  expect(status.mock.calls).toEqual([[true], [false]]);
  socket.ended();
  vi.advanceTimersByTime(20_000);
  expect(Socket.instances).toHaveLength(1);
  expect(vi.getTimerCount()).toBe(0);
});

it("网络断线按退避重连，换身份会取消尚未执行的重连", () => {
  const client = new WsClient();
  client.connect("alice");
  Socket.instances[0]!.opened();
  Socket.instances[0]!.ended();
  vi.advanceTimersByTime(500);
  expect(Socket.instances).toHaveLength(2);
  expect(Socket.instances[1]!.protocols).toEqual(["bearer", "alice"]);
  Socket.instances[1]!.ended();
  client.connect("bob");
  vi.advanceTimersByTime(20_000);
  expect(Socket.instances).toHaveLength(3);
  expect(Socket.instances[2]!.protocols).toEqual(["bearer", "bob"]);
  client.close();
});
