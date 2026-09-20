import { useQuery } from "@tanstack/react-query";
import { HardDrive, Home, Slash } from "lucide-react";
import { useRef } from "react";
import { api } from "@/api/client";
import type { components } from "@/api/schema";
import { capabilitiesQuery } from "@/app/queries";
import { cx } from "@/lib/cx";
import { fmtBytes } from "@/lib/fmt";
import { useSession } from "@/session/useSession";
import { guessHome, platformOf } from "./path";
import s from "./Workspace.module.css";

type FilesystemInfo = components["schemas"]["FilesystemInfo"];

/** 挂载点列表。周期性重取：挂载会来去（U 盘、NAS），也要发现「卡死后消失」的。 */
function filesystemsQuery() {
  return {
    queryKey: ["system", "info", "filesystems"],
    queryFn: async (): Promise<FilesystemInfo[]> => {
      const { data, error } = await api.GET("/api/v1/system/info");
      if (error) throw error;
      return data.filesystems ?? [];
    },
    refetchInterval: 30_000,
  } as const;
}

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

  // 本页生命周期内见过的挂载点。用 ref 而不是 state：它只在渲染时被读，
  // 且只增不减，不需要触发额外渲染。
  const seen = useRef(new Map<string, FilesystemInfo>());
  for (const f of fs.data ?? []) seen.current.set(f.mount_point, f);
  const liveMounts = fs.data ?? [];
  const liveKeys = new Set(liveMounts.map((f) => f.mount_point));
  // 只有在**拿到过一次成功结果**之后，缺席才有意义；请求失败时不判缺席。
  const staleMounts =
    fs.data === undefined
      ? []
      : [...seen.current.values()].filter((f) => !liveKeys.has(f.mount_point));

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
