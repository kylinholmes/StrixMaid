import type { ButtonHTMLAttributes, ReactNode } from "react";
import { cx } from "@/lib/cx";
import s from "./Button.module.css";

export type ButtonVariant = "secondary" | "primary" | "danger";

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  size?: "md" | "sm";
  /** 只有图标、没有文字时必须给 aria-label */
  iconOnly?: boolean;
  children?: ReactNode;
}

const VARIANT: Record<ButtonVariant, string | undefined> = {
  secondary: "",
  primary: s.primary,
  danger: s.danger,
};

/**
 * 普通按钮没有品牌色（spec §2.1）：主按钮是 ink 实心，次按钮是 1px 描边。
 * `danger` 是状态色进入界面层的唯一例外，只允许用在破坏性操作上，
 * 且**必须**配二次确认框。
 */
export function Button({
  variant = "secondary",
  size = "md",
  iconOnly = false,
  className,
  type = "button",
  ...rest
}: ButtonProps) {
  const cls = cx(s.btn, VARIANT[variant], size === "sm" && s.sm, iconOnly && s.icon, className);
  return <button type={type} className={cls} {...rest} />;
}
