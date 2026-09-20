/**
 * `WS /ws/terminal/{id}` 的薄客户端（`docs/roadmap/12-workspace.md` §4.3）。
 *
 * **不复用 `lib/ws.ts`**：那是 `/ws` 控制面的 envelope 多路复用客户端，而终端是
 * 裸二进制流——二进制帧就是 PTY 字节，原样透传，包一层 JSON 就是每个按键付一次
 * base64 的税。协议见 `crates/strixmaid-node/src/ws/terminal.rs`：
 *
 * - 双向二进制帧：PTY 原始字节；
 * - 客户端 → 服务端文本：`{"t":"resize","cols":N,"rows":M}`；
 * - 服务端 → 客户端文本：`{"t":"exit","reason":…,"code"?,"signal"?}`，随后关连接。
 *
 * **退出与断线是两件事**（§4.3，`/debug` 原型错在这里）：收到 exit 帧说明终端
 * 本身没了；没收到 exit 的关闭只是这条 WS 断了、PTY 还在跑，可以重连恢复。
 * 因此 exit 之后的 onclose 不再报断线，主动 `close()` 也不报——只有「意外断开
 * 且终端还活着」才走 `onStatus(false)`。
 *
 * 不做自动重连：断线后该不该重连、什么时候重连是界面的决定（用户可能正看着
 * 「已退出」的横幅），这里只上报事实。
 */

/** 服务端的 `{"t":"exit"}` 帧。`code` / `signal` 只在 worker 真取到退出状态时出现。 */
export interface ExitFrame {
  reason: string;
  code?: number;
  signal?: number;
}

export interface TermSocketHandlers {
  /** PTY 的一段原始输出字节。 */
  onData(bytes: Uint8Array): void;
  /** 终端本身结束了（shell 退出 / 被删 / 空闲回收……）。 */
  onExit(frame: ExitFrame): void;
  /** 连接状态：`true` 已连上；`false` 意外断开且终端仍在（可重连）。 */
  onStatus(up: boolean): void;
}

/** 与 `session/useSession.ts` 用同一个键——termsocket 不反向依赖 store。 */
const TOKEN_KEY = "strixmaid.session.token";

export class TermSocket {
  private ws: WebSocket | null = null;
  private exited = false;
  private closedByUs = false;

  constructor(id: string, h: TermSocketHandlers) {
    const token = globalThis.localStorage?.getItem(TOKEN_KEY) ?? "";
    const loc = globalThis.location;
    const proto = loc?.protocol === "https:" ? "wss" : "ws";
    const ws = new WebSocket(`${proto}://${loc?.host ?? ""}/ws/terminal/${id}`, ["bearer", token]);
    ws.binaryType = "arraybuffer";
    this.ws = ws;

    ws.onopen = () => h.onStatus(true);
    ws.onmessage = (ev) => {
      if (ev.data instanceof ArrayBuffer) {
        h.onData(new Uint8Array(ev.data));
        return;
      }
      // 文本帧只有 exit 一种；解析不了就丢弃（一个坏帧不该断掉终端）。
      try {
        const frame = JSON.parse(String(ev.data)) as { t?: string } & ExitFrame;
        if (frame.t === "exit") {
          this.exited = true;
          const { reason, code, signal } = frame;
          h.onExit({
            reason,
            ...(code !== undefined && { code }),
            ...(signal !== undefined && { signal }),
          });
        }
      } catch {
        // 忽略
      }
    };
    ws.onclose = () => {
      this.ws = null;
      if (!this.exited && !this.closedByUs) h.onStatus(false);
    };
    ws.onerror = () => {
      // onclose 会跟着来，事实上报在那边。
    };
  }

  /** 把键盘字节写进 PTY。连接未就绪时静默丢弃（终端语义下重发没有意义）。 */
  send(bytes: Uint8Array): void {
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(bytes);
  }

  /** 改窗口大小。 */
  resize(cols: number, rows: number): void {
    if (this.ws?.readyState === WebSocket.OPEN)
      this.ws.send(JSON.stringify({ t: "resize", cols, rows }));
  }

  /** 主动解除附着（切走/卸载）。终端继续跑，重连即恢复。 */
  close(): void {
    this.closedByUs = true;
    this.ws?.close();
    this.ws = null;
  }
}
