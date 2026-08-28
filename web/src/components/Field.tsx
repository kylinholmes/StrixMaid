import { type InputHTMLAttributes, type ReactNode, useId } from "react";
import { cx } from "@/lib/cx";
import s from "./Field.module.css";

export interface FieldProps extends Omit<InputHTMLAttributes<HTMLInputElement>, "id" | "prefix"> {
  label: string;
  hint?: string;
  error?: string;
  /** 输入框左侧的固定前缀，例如单位或提示符 */
  leading?: ReactNode;
}

/**
 * 输入框。焦点样式由 :focus-visible 全局提供（spec §5.4），
 * 这里只把边框在 focus-within 时提到 ink——两者叠加不冲突：
 * outline 画在框外 2px 处，边框在框上。
 */
export function Field({ label, hint, error, leading, className, ...rest }: FieldProps) {
  const id = useId();
  const describedBy = error ? `${id}-err` : hint ? `${id}-hint` : undefined;
  return (
    <div className={cx(s.wrap, className)}>
      <label className={s.label} htmlFor={id}>
        {label}
      </label>
      <div className={s.box}>
        {leading}
        <input id={id} className={s.input} aria-describedby={describedBy} {...rest} />
      </div>
      {error ? (
        <span id={`${id}-err`} className={cx(s.hint, s.error)}>
          {error}
        </span>
      ) : hint ? (
        <span id={`${id}-hint`} className={s.hint}>
          {hint}
        </span>
      ) : null}
    </div>
  );
}
