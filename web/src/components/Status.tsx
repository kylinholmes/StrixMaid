import type { ReactNode } from "react";
import { cx } from "@/lib/cx";
import s from "./Status.module.css";

export type RunState = "run" | "stop" | "fail" | "unknown";

const MARK: Record<RunState, string | undefined> = {
  run: s.run,
  stop: s.stop,
  fail: s.fail,
  unknown: s.unknown,
};

const LABEL: Record<RunState, string> = {
  run: "运行中",
  stop: "已停止",
  fail: "失败",
  unknown: "未知",
};

/**
 * 状态标记。用几何形而非图标（spec §5.6）：
 * 实心方块 = 运行、描边方块 = 停止、严重色实心 = 失败。
 *
 * **颜色从来不是唯一的身份编码**（spec §10）——所以标记旁边永远带文字。
 */
export function StatusDot({ state, label }: { state: RunState; label?: string }) {
  return (
    <span className={s.dot}>
      <span className={cx(s.mark, MARK[state])} aria-hidden="true" />
      {label ?? LABEL[state]}
    </span>
  );
}

export type TagTone = "default" | "bad" | "warn";

const TONE: Record<TagTone, string | undefined> = { default: "", bad: s.tagBad, warn: s.tagWarn };

/** 计数徽标 / 只读标签。平台名（systemd、launchd）只能以这个形态出现（spec §1）。 */
export function Tag({ children, tone = "default" }: { children: ReactNode; tone?: TagTone }) {
  return <span className={cx(s.tag, TONE[tone])}>{children}</span>;
}
