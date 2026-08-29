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
  private subs = new Map<
    string,
    { params: unknown; listeners: Set<Listener>; confirmed: boolean }
  >();
  private retry = 0;
  private closed = true;
  private nextId = 1;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private confirmTimer: ReturnType<typeof setInterval> | null = null;
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
    if (this.confirmTimer) clearInterval(this.confirmTimer);
    this.confirmTimer = null;
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
      entry = { params, listeners: new Set(), confirmed: false };
      this.subs.set(ch, entry);
      this.sendSub(ch, entry.params);
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
      console.info("[ws] 已连接，补发订阅", [...this.subs.keys()]);
      this.retry = 0;
      for (const fn of this.statusListeners) fn(true);
      // 订阅必须收到确认（resp 或首帧 data）。经代理的连接上，onopen 里
      // 立刻发出的帧可能被代理丢掉（http-proxy 接管套接字的竞态），
      // 所以未确认的订阅每 1.5s 重发一次，直到确认为止。服务端重复 sub 幂等。
      for (const e of this.subs.values()) e.confirmed = false;
      this.flushSubs();
      if (this.confirmTimer) clearInterval(this.confirmTimer);
      this.confirmTimer = setInterval(() => {
        if (this.ws?.readyState !== WebSocket.OPEN) return;
        const pending = [...this.subs.entries()].filter(([, e]) => !e.confirmed);
        if (pending.length === 0) return;
        console.warn(
          "[ws] 订阅未确认，重发",
          pending.map(([ch]) => ch),
        );
        this.flushSubs();
      }, 1_500);
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
        if (e) {
          e.confirmed = true;
          for (const fn of e.listeners) fn(env.d);
        }
      } else if (env.t === "resp" && env.ch) {
        const e = this.subs.get(env.ch);
        if (e) e.confirmed = true;
      } else if (env.t === "err") {
        // 服务端拒绝（订阅参数不合法、未知频道……）不能静默——这是排查的唯一线索
        console.error("[ws] 服务端错误帧", env.ch ?? "(连接级)", env.d);
      }
    };
    ws.onclose = (ev) => {
      console.info("[ws] 连接关闭", ev.code, ev.reason || "(无原因)");
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

  private flushSubs(): void {
    for (const [ch, e] of this.subs) {
      if (!e.confirmed) this.send({ v: 1, t: "sub", ch, id: this.nextId++, d: e.params });
    }
  }

  private sendSub(ch: string, params: unknown): void {
    this.send({ v: 1, t: "sub", ch, id: this.nextId++, d: params });
  }

  private send(env: Envelope): void {
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(env));
  }
}

/** 全局唯一实例：会话 store 在登录/锁定时 connect/close。 */
export const wsClient = new WsClient();
