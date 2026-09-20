import { useQuery } from "@tanstack/react-query";
import { HardDrive, Home, Slash } from "lucide-react";
import { api } from "@/api/client";
import type { components } from "@/api/schema";
import { capabilitiesQuery } from "@/app/queries";
import { cx } from "@/lib/cx";
import { fmtBytes } from "@/lib/fmt";
import { useSession } from "@/session/useSession";
import { guessHome, platformOf } from "./path";
import s from "./Workspace.module.css";

type FilesystemInfo = components["schemas"]["FilesystemInfo"];
type SystemInfo = components["schemas"]["SystemInfo"];

/**
 * 挂载点列表。**复用全局的 `["system","info"]` 缓存键**（概览页与性能页
 * 已在用同一个端点），只是这里加了周期重取——挂载会来去（U 盘、NAS），
 * 也要发现「卡死后消失」的。别开新键：同一端点两个键就是双倍请求
 * 加两份会打架的缓存。
 */
function filesystemsQuery() {
  return {
    queryKey: ["system", "info"],
    queryFn: async (): Promise<SystemInfo> => {
      const { data, error } = await api.GET("/api/v1/system/info");
      if (error) throw error;
      return data;
    },
    refetchInterval: 30_000,
    select: (d: SystemInfo) => d.filesystems ?? [],
  } as const;
}

/**
 * 会话生命周期内见过的挂载点（§4.2 的「无响应」判定基线）。
 *
 * **模块级**而不是组件内的 ref：快速访问栏折叠、或切去别的页面时组件会
 * 卸载，记忆跟着组件走的话，回来那一刻基线清零，卡死的挂载正好「凭空
 * 消失」——恰是这条需求要防的事。
 */
const seenMounts = new Map<string, FilesystemInfo>();

export interface QuickAccessProps {
  current: string | null;
  onGo: (path: string) => void;
}

/**
 * 快速访问（§4.2）：已知文件夹 + 驱动器/挂载点，两段的数据源都是现成的。
 *
 * **卡死的挂载不能凭空消失**：Linux 侧对每个挂载点 `statvfs`，失败即整条被丢掉
 * （实测结论，§2.3）。这里对「上一次见过、这一次没了」的挂载点保留一条灰显条目
 * 并标「无响应」——消失会让人以为挂载被卸载了。
 */
export function QuickAccess({ current, onGo }: QuickAccessProps) {
  const caps = useQuery(capabilitiesQuery());
  const user = useSession((st) => st.user);
  const fs = useQuery(filesystemsQuery());

  for (const f of fs.data ?? []) seenMounts.set(f.mount_point, f);
  const liveMounts = fs.data ?? [];
  const liveKeys = new Set(liveMounts.map((f) => f.mount_point));
  // 只有在**拿到过一次成功结果**之后，缺席才有意义；请求失败时不判缺席。
  const staleMounts =
    fs.data === undefined
      ? []
      : [...seenMounts.values()].filter((f) => !liveKeys.has(f.mount_point));

  const osId = caps.data?.identity?.os_id;
  const platform = platformOf(osId);
  const home = user ? guessHome(osId, user.username, user.uid) : null;
  const root = platform === "windows" ? "\\" : "/";

  const item = (
    path: string,
    label: string,
    icon: React.ReactNode,
    extra?: React.ReactNode,
    stale = false,
  ) => (
    <button
      key={path}
      type="button"
      className={cx(s.railItem, current === path && s.railItemActive, stale && s.railStale)}
      onClick={stale ? undefined : () => onGo(path)}
      title={path}
    >
      {icon}
      <span className={s.railItemName}>{label}</span>
      {extra}
    </button>
  );

  return (
    <nav className={s.rail} aria-label="快速访问">
      <div className={s.railGroup}>
        <span className={s.railLabel}>位置</span>
        {home && item(home, "主目录", <Home size={14} strokeWidth={1.5} />)}
        {item(
          root,
          platform === "windows" ? "全部驱动器" : "根目录",
          <Slash size={14} strokeWidth={1.5} />,
        )}
      </div>
      <div className={s.railGroup}>
        <span className={s.railLabel}>{platform === "windows" ? "驱动器" : "挂载点"}</span>
        {liveMounts.map((f) => (
          <div key={f.mount_point}>
            {item(
              f.mount_point,
              f.mount_point,
              <HardDrive size={14} strokeWidth={1.5} />,
              <span className={s.staleTag}>{fmtBytes(f.available_bytes)} 可用</span>,
            )}
            <div className={s.railUsage} aria-hidden="true">
              <i
                style={{
                  width: `${Math.min(100, (f.used_bytes / Math.max(1, f.total_bytes)) * 100)}%`,
                }}
              />
            </div>
          </div>
        ))}
        {staleMounts.map((f) =>
          item(
            f.mount_point,
            f.mount_point,
            <HardDrive size={14} strokeWidth={1.5} />,
            <span className={s.staleTag}>无响应</span>,
            true,
          ),
        )}
      </div>
    </nav>
  );
}
