import { cx } from "@/lib/cx";
import s from "./Segmented.module.css";

export interface SegmentedOption<T extends string> {
  value: T;
  label: string;
}

export interface SegmentedProps<T extends string> {
  options: readonly SegmentedOption<T>[];
  value: T;
  onChange: (value: T) => void;
  label: string;
}

/** 分段控件。用 aria-pressed 而不是 radio —— 它是一组互斥的按钮，不是表单字段。 */
export function Segmented<T extends string>({
  options,
  value,
  onChange,
  label,
}: SegmentedProps<T>) {
  return (
    <fieldset className={s.wrap}>
      <legend className={s.legend}>{label}</legend>
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          className={cx(s.item, o.value === value && s.on)}
          aria-pressed={o.value === value}
          onClick={() => onChange(o.value)}
        >
          {o.label}
        </button>
      ))}
    </fieldset>
  );
}
