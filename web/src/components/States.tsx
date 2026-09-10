import type { ReactNode } from "react";
import { Button } from "./Button";
import s from "./States.module.css";

/** 首次进入时的骨架屏。刷新已有数据时**不要**用它（spec §6）。 */
export function TableSkeleton({
  rows = 8,
  widths = [150, 74, 54, 96],
}: {
  rows?: number;
  widths?: readonly number[];
}) {
  return (
    <div className={s.skeleton} role="status" aria-busy="true" aria-label="正在读取">
      {Array.from({ length: rows }, (_, i) => (
        // biome-ignore lint/suspicious/noArrayIndexKey: 纯装饰占位，无身份
        <div className={s.skelRow} key={i}>
          {widths.map((w, j) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: 同上
            <span className={s.bar} key={j} style={{ width: `${w - (i % 3) * 12}px` }} />
          ))}
        </div>
      ))}
    </div>
  );
}

/** 刷新已有数据时的进度线。数据留在原地，只有这条 2px 的线在动。 */
export function ProgressLine() {
  return (
    <div className={s.progress} role="progressbar" aria-label="正在刷新">
      <i />
    </div>
  );
}

export interface EmptyStateProps {
  /** 说清**为什么**空，不要写「暂无数据」——那是系统在描述自己（spec §6） */
  title: string;
  detail?: string;
  actionLabel?: string;
  onAction?: () => void;
}

export function EmptyState({ title, detail, actionLabel, onAction }: EmptyStateProps) {
  return (
    <div className={s.center}>
      <span className={s.title}>{title}</span>
      {detail && <span className={s.detail}>{detail}</span>}
      {actionLabel && onAction && (
        <span className={s.action}>
          <Button onClick={onAction}>{actionLabel}</Button>
        </span>
      )}
    </div>
  );
}

export interface ErrorStateProps {
  /** 发生了什么。不道歉、不含糊 */
  title: string;
  detail?: ReactNode;
  /** 可执行的建议。**由服务端按探测到的平台下发**，前端不硬编码任何平台的命令 */
  command?: string;
  onRetry?: () => void;
}

export function ErrorState({ title, detail, command, onRetry }: ErrorStateProps) {
  return (
    <div>
      <div className={s.error} role="alert">
        <span className={s.stripe} aria-hidden="true" />
        <div className={s.text}>
          <strong>{title}</strong>
          {detail ? <> {detail}</> : null}
          {command && <code className={s.cmd}>{command}</code>}
        </div>
      </div>
      {onRetry && (
        <div style={{ padding: "0 14px 14px" }}>
          <Button onClick={onRetry}>重试</Button>
        </div>
      )}
    </div>
  );
}
