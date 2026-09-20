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

  const lastDims = useRef<{ cols: number; rows: number } | null>(null);
  const fitPending = useRef(false);

  const fitAndReport = useCallback(() => {
    const term = termRef.current;
    const fit = fitRef.current;
    if (!term || !fit) return;
    fit.fit();
    // 尺寸没变就不发帧：拖面板边时 ResizeObserver 每帧都响，把没变化的
    // resize 也发出去，就是「每个鼠标事件 × 每个标签一次 worker RPC」的风暴。
    const dims = { cols: term.cols, rows: term.rows };
    if (lastDims.current?.cols === dims.cols && lastDims.current?.rows === dims.rows) return;
    lastDims.current = dims;
    sockRef.current?.resize(dims.cols, dims.rows);
  }, []);

  /** 一帧最多量一次（rAF 合并），进一步压掉拖动期间的连环 fit。 */
  const scheduleFit = useCallback(() => {
    if (fitPending.current) return;
    fitPending.current = true;
    requestAnimationFrame(() => {
      fitPending.current = false;
      fitAndReport();
    });
  }, [fitAndReport]);

  const connect = useCallback(() => {
    sockRef.current?.close();
    const sock = new TermSocket(id, {
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
    sockRef.current = sock;
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
      // 只有活动标签才响应：非活动标签是 visibility:hidden，布局还在、
      // RO 照样触发，8 个标签一起 fit 就是拖面板边时的卡顿来源。
      // 隐藏期间漏掉的尺寸变化由「切回来补一次 fit」兜住。
      if (!activeRef.current) return;
      // 尺寸为 0（面板折叠）时 fit 会量出胡话，跳过。
      if (host.clientWidth > 0 && host.clientHeight > 0) scheduleFit();
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
  }, [connect, scheduleFit]);

  // 活动状态给 RO 回调用（RO 的闭包建于挂载时，直接读 prop 是旧值）。
  const activeRef = useRef(active);
  activeRef.current = active;

  // 切回来时补一次 fit（隐藏期间的容器尺寸变化被上面的 RO 门挡掉了）。
  useEffect(() => {
    if (active) requestAnimationFrame(fitAndReport);
  }, [active, fitAndReport]);

  // 面板从折叠展开时聚焦终端（⌃` 打开即打字，VSCode 同款）。
  const collapsed = useWorkspace((st) => st.panelCollapsed);
  useEffect(() => {
    if (!collapsed && active) termRef.current?.focus();
  }, [collapsed, active]);

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
