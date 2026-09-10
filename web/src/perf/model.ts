import type { Discovery } from "@/metrics/discovery";

export type ResourceId = "cpu" | "gpu" | "mem" | "disk" | "net";

export interface ResourceDef {
  readonly id: ResourceId;
  readonly label: string;
  /** 数据层资源色变量 */
  readonly tone: string;
  /** 任一前缀有序列即显示（08 §6.1 退化规则：0 成员整类隐藏） */
  readonly probes: readonly string[];
  /** 该行压力带的 PSI 指标；无 = 不画（不是画恒绿，08 §6.7） */
  readonly psi?: string;
  /** 成员标签键（组资源才有） */
  readonly memberLabel?: string;
  /** 成员的主指标（成员格小图与成员清单来源） */
  readonly memberMetric?: string;
}

export const RESOURCES: readonly ResourceDef[] = [
  { id: "cpu", label: "CPU", tone: "--cpu", probes: ["cpu."], psi: "psi.cpu.some" },
  {
    id: "gpu",
    label: "GPU",
    tone: "--gpu",
    probes: ["gpu."],
    memberLabel: "gpu",
    memberMetric: "gpu.usage",
  },
  { id: "mem", label: "内存", tone: "--mem", probes: ["mem."], psi: "psi.memory.some" },
  {
    id: "disk",
    label: "磁盘",
    tone: "--disk",
    probes: ["disk.", "fs."],
    psi: "psi.io.some",
    memberLabel: "dev",
    memberMetric: "disk.util",
  },
  {
    id: "net",
    label: "网络",
    tone: "--net",
    probes: ["net."],
    memberLabel: "iface",
    memberMetric: "net.tx_bytes",
  },
];

export function visibleResources(d: Discovery | undefined): readonly ResourceDef[] {
  if (!d) return [];
  return RESOURCES.filter((r) => r.probes.some((p) => d.has(p)));
}
