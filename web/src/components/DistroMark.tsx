import { cx } from "@/lib/cx";
import type { Distro } from "@/theme/distro";
import s from "./DistroMark.module.css";

/**
 * 发行版标记：色块 + 首字母。
 *
 * **不使用发行版的商标图形**（spec §5.6）——那些标识各有商标政策。
 * 首字母让它在色盲下也能认，颜色不是唯一编码。
 */
export function DistroMark({ distro, size = "md" }: { distro: Distro; size?: "md" | "lg" }) {
  const cls = cx(s.mark, size === "lg" && s.lg, !distro.brand && s.unknown);
  return (
    <span
      className={cls}
      style={distro.brand ? { background: distro.brand } : undefined}
      title={distro.name}
      aria-hidden="true"
    >
      {distro.initial}
    </span>
  );
}
