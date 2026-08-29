/**
 * `/ws` 控制面客户端（design.md §9）。
 *
 * - 鉴权：token 走 WebSocket 子协议 `["bearer", <token>]`，升级前完成；
 * - envelope：`{v:1, t, ch, id?, d}`，字段单字母是协议约定（高频小包）；
 * - 断线指数退避重连，重连成功后自动补发全部订阅。
 */

interface Envelope {
  v: number;
  t: "sub" | "unsub" | "data" | "req" | "resp" | "err";
  ch?: string;
  id?: number;
  d?: unknown;
}

type Listener = (payload: unknown) => void;

export class WsClient {
  private ws: WebSocket | null = null;
  private token: string | null = null;
  private subs = new Map<string, { params: unknown; listeners: Set<Listener> }>();
  private retry = 0;
  private closed = true;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private statusListeners = new Set<(up: boolean) => void>();

  /** 连接（或换 token 重连）。 */
  connect(token: string): void {
    this.token = token;
    this.closed = false;
    this.open();
  }

  /** 主动关闭（锁定时调用），不再重连。 */
  close(): void {
    this.closed = true;
    this.token = null;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.ws?.close();
    this.ws = null;
  }

  get up(): boolean {
    return this.ws?.readyState === WebSocket.OPEN;
  }

  onStatus(fn: (up: boolean) => void): () => void {
    this.statusListeners.add(fn);
    return () => this.statusListeners.delete(fn);
  }

  /** 订阅频道。返回退订函数。同频道多处订阅共享一条服务端订阅。 */
  subscribe(ch: string, params: unknown, listener: Listener): () => void {
    let entry = this.subs.get(ch);
    if (!entry) {
      entry = { params, listeners: new Set() };
      this.subs.set(ch, entry);
      this.send({ v: 1, t: "sub", ch, d: params });
    }
    entry.listeners.add(listener);
    return () => {
      const e = this.subs.get(ch);
      if (!e) return;
      e.listeners.delete(listener);
      if (e.listeners.size === 0) {
        this.subs.delete(ch);
        this.send({ v: 1, t: "unsub", ch, d: {} });
      }
    };
  }

  private open(): void {
    if (!this.token) return;
    const proto = location.protocol === "https:" ? "wss" : "ws";
    const ws = new WebSocket(`${proto}://${location.host}/ws`, ["bearer", this.token]);
    this.ws = ws;

    ws.onopen = () => {
      this.retry = 0;
      for (const fn of this.statusListeners) fn(true);
      // 重连后补发订阅
      for (const [ch, e] of this.subs) this.send({ v: 1, t: "sub", ch, d: e.params });
    };
    ws.onmessage = (ev) => {
      let env: Envelope;
      try {
        env = JSON.parse(String(ev.data)) as Envelope;
      } catch {
        return;
      }
      if (env.t === "data" && env.ch) {
        const e = this.subs.get(env.ch);
        if (e) for (const fn of e.listeners) fn(env.d);
      }
    };
    ws.onclose = () => {
      for (const fn of this.statusListeners) fn(false);
      this.ws = null;
      if (this.closed) return;
      const delay = Math.min(15_000, 500 * 2 ** this.retry++);
      this.reconnectTimer = setTimeout(() => this.open(), delay);
    };
    ws.onerror = () => {
      // onclose 会跟着来，重连逻辑在那边
    };
  }

  private send(env: Envelope): void {
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(env));
  }
}

/** 全局唯一实例：会话 store 在登录/锁定时 connect/close。 */
export const wsClient = new WsClient();
