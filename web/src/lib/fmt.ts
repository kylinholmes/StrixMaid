/** 数据呈现约定（spec §9）：字节 1024 进制且明确标 GiB/TiB；网络速率例外用 1000 进制 b/s。 */

const BYTE_UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"] as const;

export function fmtBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  let v = bytes;
  let i = 0;
  while (v >= 1024 && i < BYTE_UNITS.length - 1) {
    v /= 1024;
    i += 1;
  }
  const digits = v >= 100 || i === 0 ? 0 : 1;
  return `${v.toFixed(digits)} ${BYTE_UNITS[i]}`;
}

const BIT_UNITS = ["b/s", "Kb/s", "Mb/s", "Gb/s", "Tb/s"] as const;

/** 网络速率：字节每秒 → 比特每秒，1000 进制（惯例例外，spec §9）。 */
export function fmtRateBits(bytesPerSec: number): string {
  if (!Number.isFinite(bytesPerSec) || bytesPerSec < 0) return "—";
  let v = bytesPerSec * 8;
  let i = 0;
  while (v >= 1000 && i < BIT_UNITS.length - 1) {
    v /= 1000;
    i += 1;
  }
  const digits = v >= 100 || i === 0 ? 0 : 1;
  return `${v.toFixed(digits)} ${BIT_UNITS[i]}`;
}

export function fmtUptime(secs: number): string {
  if (!Number.isFinite(secs) || secs < 0) return "—";
  const d = Math.floor(secs / 86_400);
  const h = Math.floor((secs % 86_400) / 3_600);
  const m = Math.floor((secs % 3_600) / 60);
  if (d > 0) return `${d} 天 ${h} 时`;
  if (h > 0) return `${h} 时 ${m} 分`;
  return `${m} 分`;
}

export function fmtPct(fraction: number): string {
  if (!Number.isFinite(fraction)) return "—";
  return `${Math.round(fraction * 100)}%`;
}
