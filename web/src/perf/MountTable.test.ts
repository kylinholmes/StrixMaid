import { describe, expect, it } from "vitest";
import type { components } from "@/api/schema";
import { mountTotals } from "./MountTable";

type Fs = components["schemas"]["FilesystemInfo"];

const GIB = 1024 ** 3;

function fs(mount: string, dev: string | null, totalGiB: number, usedGiB: number): Fs {
  return {
    mount_point: mount,
    backing_dev: dev,
    device: dev ? `/dev/${dev}` : mount,
    fs_type: dev ? "apfs" : "tmpfs",
    read_only: false,
    total_bytes: totalGiB * GIB,
    used_bytes: usedGiB * GIB,
    available_bytes: (totalGiB - usedGiB) * GIB,
  };
}

const NO_RINGS = new Map();

describe("mountTotals", () => {
  it("APFS 共享容器:各卷报的都是容器级 used/total,只数一次", () => {
    // 曾经的 bug:5 卷 × 284 GiB 相加 → 153%。同容器 used 取一次。
    const t = mountTotals(
      [
        fs("/", "disk3", 926, 284),
        fs("/System/Volumes/Data", "disk3", 926, 284),
        fs("/System/Volumes/VM", "disk3", 926, 283),
      ],
      NO_RINGS,
    );
    expect(t.total).toBe(926 * GIB);
    expect(t.used).toBe(284 * GIB);
    expect(t.devCount).toBe(1);
  });

  it("Linux 真分区(同盘 total 不等):各自相加", () => {
    const t = mountTotals(
      [fs("/", "nvme0n1", 400, 100), fs("/boot/efi", "nvme0n1", 1, 0.2)],
      NO_RINGS,
    );
    expect(t.total).toBe(401 * GIB);
    expect(t.used).toBeCloseTo(100.2 * GIB, -3);
    expect(t.devCount).toBe(1);
  });

  it("无块设备的挂载(tmpfs 等)按各自的账相加,不计设备数", () => {
    const t = mountTotals([fs("/run", null, 2, 1), fs("/tmp", null, 2, 0.5)], NO_RINGS);
    expect(t.total).toBe(4 * GIB);
    expect(t.used).toBeCloseTo(1.5 * GIB, -3);
    expect(t.devCount).toBe(0);
  });

  it("混合:两块盘 + 伪文件系统,设备数只数真设备", () => {
    const t = mountTotals(
      [
        fs("/", "disk3", 926, 284),
        fs("/System/Volumes/Data", "disk3", 926, 284),
        fs("/Volumes/ext", "disk4", 100, 50),
        fs("/run", null, 2, 1),
      ],
      NO_RINGS,
    );
    expect(t.devCount).toBe(2);
    expect(t.total).toBe((926 + 100 + 2) * GIB);
    expect(t.used).toBe((284 + 50 + 1) * GIB);
  });
});
