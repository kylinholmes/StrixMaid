import {
  Activity,
  ClipboardList,
  FileText,
  Folder,
  LayoutGrid,
  ListTree,
  type LucideIcon,
  Server,
  Settings,
  SquareTerminal,
} from "lucide-react";
import type { components } from "@/api/schema";

export type SystemCaps = components["schemas"]["SystemCapabilities"];

/**
 * 页面 = 能力的展示面。`cap` 指向支撑它的机器级能力：探测不到就**整个不出现**
 * （spec §6——不是灰掉，是不存在）；`cap: null` 的页面不依赖探测。
 *
 * `via` 是登录门页面清单右侧的来源标签（子系统名，不是平台方言）。
 */
export interface PageDef {
  readonly id: string;
  readonly label: string;
  readonly icon: LucideIcon;
  readonly cap: keyof SystemCaps | null;
  readonly via: string;
}

export interface PageGroup {
  readonly label: string;
  readonly items: readonly PageDef[];
}

export const PAGE_GROUPS: readonly PageGroup[] = [
  {
    label: "观测",
    items: [
      { id: "overview", label: "概览", icon: LayoutGrid, cap: null, via: "system" },
      { id: "performance", label: "性能", icon: Activity, cap: null, via: "metrics" },
      { id: "processes", label: "进程", icon: ListTree, cap: null, via: "proc" },
      { id: "services", label: "服务", icon: Server, cap: "systemd", via: "services" },
      { id: "logs", label: "日志", icon: FileText, cap: "journal", via: "journal" },
    ],
  },
  {
    label: "操作",
    items: [
      { id: "terminal", label: "终端", icon: SquareTerminal, cap: "helper", via: "pty" },
      { id: "files", label: "文件", icon: Folder, cap: "helper", via: "worker" },
    ],
  },
  {
    label: "系统",
    items: [
      { id: "audit", label: "审计", icon: ClipboardList, cap: null, via: "audit" },
      { id: "settings", label: "设置", icon: Settings, cap: null, via: "内建" },
    ],
  },
];

export function pageAvailable(page: PageDef, caps: SystemCaps | undefined): boolean {
  if (page.cap === null) return true;
  // 能力未知（还没拿到 /capabilities）时先当可用，拿到后再收
  return caps ? caps[page.cap] : true;
}
