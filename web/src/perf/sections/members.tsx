import { useRef } from "react";
import { useNavigate } from "react-router-dom";
import { cssVar, Plot, withAlpha } from "@/components/Plot";
import { cx } from "@/lib/cx";
import { seriesKey, useLive } from "@/metrics/live";
import { useTheme } from "@/theme/useTheme";
import { liveSingle } from "../chart";
import type { ResourceDef } from "../model";
import s from "../Perf.module.css";

import { CELL_BORDER_H, CELL_MIN_H, CELL_MIN_W, GRID_GAP, gridLayout, useBoxWidth } from "./layout";

/* ================= 成员格 ================= */

/*
 * 成员格要的资源描述。与 `model.ts` 的 `RESOURCES` 同源，但那份是数组，
 * 取出来要处理「找不到」的分支；这里三条是本文件自己渲染的三段，写成常量更直白。
 */
export const DISK_RESOURCE: ResourceDef = {
  id: "disk",
  label: "磁盘",
  tone: "--disk",
  probes: ["disk."],
  memberLabel: "dev",
  memberMetric: "disk.util",
};

export const NET_RESOURCE: ResourceDef = {
  id: "net",
  label: "网络",
  tone: "--net",
  probes: ["net."],
  memberLabel: "iface",
  memberMetric: "net.tx_bytes",
};

export const GPU_RESOURCE: ResourceDef = {
  id: "gpu",
  label: "GPU",
  tone: "--gpu",
  probes: ["gpu."],
  memberLabel: "gpu",
  memberMetric: "gpu.usage",
};

/**
 * 成员格（08 §6.4）：一格一设备——迷你走势 + 名字 + 当前值 + 6% 资源色底板，
 * 点进去是该设备的详情页。它同时是「总体 ⇄ 逐设备」切换器的第二档，
 * 高度由外层弹性布局给，与总体图等高。
 *
 * 格子摊满这块面积；设备多到摊不下就退回样稿尺寸、区域内部滚动——
 * 滚动发生在格子网格里面，区块本身的高度不变，切换前后下方内容不跳。
 */
export function MemberGrid({
  resource,
  members,
  height,
  cellValue,
  tagOf,
}: {
  resource: ResourceDef;
  members: readonly string[];
  height: number;
  cellValue: (m: string) => { big: string; small?: string };
  tagOf?: (m: string) => string | null;
}) {
  const rings = useLive((st) => st.rings);
  const navigate = useNavigate();
  const hostRef = useRef<HTMLDivElement>(null);
  const w = useBoxWidth(hostRef);
  const mode = useTheme((t) => t.mode);
  void mode; // 主题切换时 cssVar 解析值变化，线色要跟着重算
  const hue = cssVar(resource.tone);
  const metric = resource.memberMetric ?? "";
  const isPct = metric === "disk.util" || metric === "gpu.usage";

  const ready = w > 0 && members.length > 0;
  const fitted = ready ? gridLayout(members.length, w, height, CELL_MIN_W, CELL_MIN_H) : null;
  const cols =
    fitted?.cols ??
    Math.max(1, Math.min(members.length, Math.floor((w + GRID_GAP) / (CELL_MIN_W + GRID_GAP))));
  const cellH = fitted?.cellH ?? CELL_MIN_H;

  return (
    <div
      ref={hostRef}
      className={cx(s.members, fitted === null && s.membersScroll)}
      style={{
        height,
        gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))`,
        gridAutoRows: `${cellH}px`,
        gap: GRID_GAP,
      }}
    >
      {!ready
        ? null
        : members.map((m) => {
            const ring = rings.get(seriesKey(metric, `${resource.memberLabel}=${m}`));
            const val = cellValue(m);
            const tag = tagOf?.(m);
            return (
              <button
                key={m}
                type="button"
                className={s.cell}
                style={{ "--tone": `var(${resource.tone})` } as React.CSSProperties}
                onClick={() => navigate(`/performance/${resource.id}/${encodeURIComponent(m)}`)}
              >
                <div className={s.cellPlot}>
                  <Plot
                    data={liveSingle(ring, 90)}
                    series={[{ stroke: hue, width: 1.5, fill: withAlpha(hue, 0.14) }]}
                    yMax={isPct ? 100 : undefined}
                    height={cellH - CELL_BORDER_H}
                    tone={resource.tone}
                    noCursor
                  />
                </div>
                <span className={s.cellName}>{m}</span>
                {tag && <span className={s.cellTag}>{tag}</span>}
                <span className={s.cellVal}>
                  {val.big}
                  {val.small && <small className={s.cellSub}>{val.small}</small>}
                </span>
              </button>
            );
          })}
    </div>
  );
}
