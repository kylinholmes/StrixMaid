import { Search } from "lucide-react";
import type { ReactNode } from "react";
import s from "./Toolbar.module.css";

export function Toolbar({ children }: { children: ReactNode }) {
  return <div className={s.bar}>{children}</div>;
}

export function ToolbarSpacer() {
  return <span className={s.spacer} />;
}

export interface SearchBoxProps {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  label: string;
}

export function SearchBox({ value, onChange, placeholder, label }: SearchBoxProps) {
  return (
    <div className={s.search}>
      <Search size={14} strokeWidth={1.3} aria-hidden="true" />
      <input
        type="search"
        value={value}
        placeholder={placeholder}
        aria-label={label}
        onChange={(e) => onChange(e.target.value)}
      />
    </div>
  );
}
