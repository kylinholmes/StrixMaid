import { useEffect, useState } from "react";

/*
 * 网格类视图（逐核小图、成员格）的几何常数。格子**摊满**容器：
 * 列数与行高由容器实测尺寸算出，这几个数是可行性的边界，不是格子的尺寸——
 * 写死格宽格高的话，容器一变高就在下面留一大片白。
 */
export const GRID_GAP = 4;
/**
 * 目标宽高比：列数在宽度允许的范围里挑一个最接近它的。
 *
 * 2.4 偏扁了——22 个接口在 1920 上只排 6 列、格子是 1.82，16 核排 4 列、
 * 格子 2.74。任务管理器的逐核格子大致在 1.4 左右。取 1.5 之后同样的容器：
 * 22 个 → 8 列（1.17）、20 核 → 7 列（1.15）、16 核 → 6 列（1.35）。
 */
export const GRID_ASPECT = 1.5;
/**
 * 宽高比的下限：比这更窄要重罚。
 *
 * 只把目标调低会在**设备少、容器高**时选出比高还窄的格子（4 个设备时算出过
 * 0.67）。这些小图的横轴是时间，压窄等于直接砍掉能看到的时间窗口——
 * 同样偏离目标，偏宽只是浪费一点面积，偏窄是丢信息，两者代价不对等。
 * 所以罚则是单边的：宁可偏宽也不偏窄。
 */
export const GRID_MIN_ASPECT = 1;
/** 越过下限之后每单位的罚分。取 4 使它足以压过 [`GRID_RAGGED`] 的末行惩罚 */
export const GRID_NARROW_PENALTY = 4;
/**
 * 末行缺几格的代价，按「缺掉的格数占一行的比例」计。
 * 只看宽高比的话，4 块盘会被摆成 3 列 ×2 行——上面一排三个、下面孤零零一个，
 * 而 2×2 明明齐整。缺 2/3 行比缺 2/12 行难看得多，所以按比例而不是按个数罚。
 */
export const GRID_RAGGED = 1.5;

/** 逐核格子里除小图外的那一截：上内边距 2 + 标签盒 15 + 上下边框 2（见 `.core`） */
export const CORE_CHROME_H = 19;
/** 逐核小图再矮就看不出波形，此时这块面积装不下一核一图 */
export const CORE_MIN_PLOT_H = 26;
/** 逐核格子最窄多少像素还画得出波形 */
export const CORE_MIN_W = 116;
/** 成员格的边框占掉的高度（`.cell` 的上下各一道），小图铺满其余部分 */
export const CELL_BORDER_H = 2;
/** 成员格的下限尺寸：与样稿一致，摊不下时就退回它并让区域内滚动 */
export const CELL_MIN_W = 132;
export const CELL_MIN_H = 82;

export interface GridLayout {
  cols: number;
  cellH: number;
}

/**
 * 在宽度允许的列数里挑一种摆法：行高由容器高度整除得出，格子摊满这块面积。
 * 挑的是「宽高比接近 `GRID_ASPECT`、末行又不太空」的那一种——纯按宽度铺满会得到
 * 又高又窄的格子，波形在里面看不出起伏。一种都摆不下（行高低于下限）时返回 `null`。
 */
export function gridLayout(
  n: number,
  w: number,
  h: number,
  minCellW: number,
  minCellH: number,
): GridLayout | null {
  const maxCols = Math.max(1, Math.min(n, Math.floor((w + GRID_GAP) / (minCellW + GRID_GAP))));
  let best: (GridLayout & { score: number }) | null = null;
  for (let cols = 1; cols <= maxCols; cols++) {
    const rows = Math.ceil(n / cols);
    const cellW = (w - GRID_GAP * (cols - 1)) / cols;
    const cellH = Math.floor((h - GRID_GAP * (rows - 1)) / rows);
    if (cellH < minCellH) continue;
    const aspect = cellW / cellH;
    const narrow = aspect < GRID_MIN_ASPECT ? (GRID_MIN_ASPECT - aspect) * GRID_NARROW_PENALTY : 0;
    const score =
      Math.abs(aspect - GRID_ASPECT) + narrow + (GRID_RAGGED * (cols * rows - n)) / cols;
    if (best === null || score < best.score) best = { cols, cellH, score };
  }
  return best === null ? null : { cols: best.cols, cellH: best.cellH };
}

/** 容器宽度：网格的列数与行高都要先知道它，而它由弹性布局给，只能量。 */
export function useBoxWidth(ref: React.RefObject<HTMLElement | null>): number {
  const [w, setW] = useState(0);
  useEffect(() => {
    const host = ref.current;
    if (!host) return;
    const ro = new ResizeObserver(() => setW(host.clientWidth));
    ro.observe(host);
    setW(host.clientWidth);
    return () => ro.disconnect();
  }, [ref]);
  return w;
}
