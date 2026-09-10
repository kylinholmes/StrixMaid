import { type ReactNode, useEffect, useId, useRef } from "react";
import { Button } from "./Button";
import s from "./Overlay.module.css";

export interface DialogProps {
  open: boolean;
  title: string;
  /** 说清后果——哪个信号、会丢什么，不要只写「确定吗」（spec §5.1） */
  children: ReactNode;
  confirmLabel: string;
  onConfirm: () => void;
  onCancel: () => void;
  /** 破坏性操作：确认按钮用 crit 实心。这是状态色进入界面层的唯一例外 */
  destructive?: boolean;
}

export function Dialog({
  open,
  title,
  children,
  confirmLabel,
  onConfirm,
  onCancel,
  destructive,
}: DialogProps) {
  const id = useId();
  const panel = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onCancel();
    };
    document.addEventListener("keydown", onKey);
    // 打开时把焦点收进对话框，否则键盘用户仍停在背后的页面上
    panel.current?.querySelector<HTMLElement>("button")?.focus();
    return () => document.removeEventListener("keydown", onKey);
  }, [open, onCancel]);

  if (!open) return null;

  return (
    <>
      {/* 遮罩只承担视觉与点击关闭；键盘关闭走上面的 Escape */}
      <div className={s.scrim} onClick={onCancel} aria-hidden="true" />
      <div
        ref={panel}
        className={s.dialog}
        role="alertdialog"
        aria-modal="true"
        aria-labelledby={`${id}-t`}
        aria-describedby={`${id}-b`}
      >
        <div className={s.head} id={`${id}-t`}>
          {title}
        </div>
        <div className={s.body} id={`${id}-b`}>
          {children}
        </div>
        <div className={s.foot}>
          <Button onClick={onCancel}>取消</Button>
          <Button variant={destructive ? "danger" : "primary"} onClick={onConfirm}>
            {confirmLabel}
          </Button>
        </div>
      </div>
    </>
  );
}
