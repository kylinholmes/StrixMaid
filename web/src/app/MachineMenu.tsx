import { ChevronDown } from "lucide-react";
import { DistroMark, Menu } from "@/components";
import { cx } from "@/lib/cx";
import { useDismiss } from "@/lib/useDismiss";
import type { Distro } from "@/theme/distro";
import s from "./MachineMenu.module.css";

/**
 * 机器行:发行版标记 + 主机名 + 下拉(当前机器 / 添加机器)。
 * 登录门与外壳侧栏共用;`wide` 是登录门里的大号形态。
 */
export function MachineMenu({
  hostname,
  distro,
  wide,
}: {
  hostname: string | undefined;
  distro: Distro;
  wide?: boolean;
}) {
  const [open, setOpen] = useDismiss();
  return (
    <div className={s.anchor}>
      <button
        type="button"
        className={cx(s.btn, wide && s.wide)}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={(e) => {
          e.stopPropagation();
          setOpen((v) => !v);
        }}
      >
        <DistroMark distro={distro} />
        <span className={s.host}>{hostname || "…"}</span>
        <ChevronDown size={12} className={s.chev} aria-hidden="true" />
      </button>
      {open && (
        <Menu
          label="机器"
          style={{
            position: "absolute",
            left: wide ? 0 : 8,
            top: "100%",
            minWidth: 236,
            zIndex: 20,
          }}
          items={[
            { id: "current", label: `${hostname ?? "本机"}（当前）`, disabled: true },
            { id: "add", label: "添加机器…", disabled: true },
          ]}
          onPick={() => setOpen(false)}
        />
      )}
    </div>
  );
}
