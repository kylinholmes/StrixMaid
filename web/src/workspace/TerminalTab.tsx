import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import { useCallback, useEffect, useRef } from "react";
import { Button } from "@/components";
import { cx } from "@/lib/cx";
import "@xterm/xterm/css/xterm.css";
import { useWorkspace } from "./store";
import { type ExitFrame, TermSocket } from "./termsocket";
import s from "./Workspace.module.css";

function exitLabel(f: ExitFrame): string {
  if (f.code !== undefined) return `已退出 (code ${f.code})`;
  if (f.signal !== undefined) return `已退出 (signal ${f.signal})`;
  return `已退出 (${f.reason})`;
}

export interface TerminalTabProps {
  id: string;
  active: boolean;
}

/**
 * 一个终端标签（§4.3）：xterm 实例 + 一条终端 WS。
 *
 * - 切走的标签**保持连接不断**：非活动时只用 CSS 隐藏，组件不卸载；
 * - 收到 exit 帧 → 标「已退出」并**保留回看内容**让人看得见最后的输出，
 *   由人手动关标签；
 * - WS 断了但没收到 exit → 「连接断开，PTY 仍在运行」，提供重连。
 *   重连前 `term.reset()`：服务端附着时会全量回放回看缓冲，不清屏会双份。
 */
export function TerminalTab({ id, active }: TerminalTabProps) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const sockRef = useRef<TermSocket | null>(null);

  const status = useWorkspace((st) => st.tabs.find((t) => t.id === id)?.status);
  const label = useWorkspace((st) => st.tabs.find((t) => t.id === id)?.exitLabel);
  const markExited = useWorkspace((st) => st.markExited);
  const markDisconnected = useWorkspace((st) => st.markDisconnected);
  const markLive = useWorkspace((st) => st.markLive);

  const fitAndReport = useCallback(() => {
    const term = termRef.current;
    const fit = fitRef.current;
    if (!term || !fit) return;
    fit.fit();
    sockRef.current?.resize(term.cols, term.rows);
  }, []);

  const connect = useCallback(() => {
    sockRef.current?.close();
    sockRef.current = new TermSocket(id, {
      onData: (bytes) => termRef.current?.write(bytes),
      onExit: (frame) => markExited(id, exitLabel(frame)),
      onStatus: (up) => {
        if (up) {
          markLive(id);
          // 附着成功后立刻把真实尺寸报给后端（创建时用的是 80x24 占位）。
          fitAndReport();
        } else {
          markDisconnected(id);
        }
      },
    });
  }, [id, markExited, markDisconnected, markLive, fitAndReport]);

  // xterm 与 WS 的生命周期各一次，与 React 渲染解耦。
  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const term = new Terminal({
      scrollback: 5_000,
      fontSize: 13,
      fontFamily: "'JetBrains Mono', 'Cascadia Mono', Menlo, Consolas, monospace",
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host);
    termRef.current = term;
    fitRef.current = fit;

    const encoder = new TextEncoder();
    const input = term.onData((data) => sockRef.current?.send(encoder.encode(data)));
    connect();

    const ro = new ResizeObserver(() => {
      // 隐藏（display 尺寸为 0）时 fit 会量出胡话，跳过。
      if (host.clientWidth > 0 && host.clientHeight > 0) fitAndReport();
    });
    ro.observe(host);

    return () => {
      ro.disconnect();
      input.dispose();
      sockRef.current?.close();
      sockRef.current = null;
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
  }, [connect, fitAndReport]);

  // 切回来时补一次 fit（隐藏期间的容器尺寸变化 ResizeObserver 量不准）。
  useEffect(() => {
    if (active) requestAnimationFrame(fitAndReport);
  }, [active, fitAndReport]);

  const reconnect = () => {
    termRef.current?.reset();
    connect();
  };

  return (
    <div className={cx(s.termHost, !active && s.termHidden)}>
      <div ref={hostRef} style={{ width: "100%", height: "100%" }} />
      {status === "exited" && (
        <div className={s.termBanner} role="status">
          <span>{label ?? "已退出"}</span>
          <span>输出已保留，关闭标签即清除。</span>
        </div>
      )}
      {status === "disconnected" && (
        <div className={s.termBanner} role="status">
          <span>连接断开，PTY 仍在运行。</span>
          <Button size="sm" onClick={reconnect}>
            重连
          </Button>
        </div>
      )}
    </div>
  );
}
