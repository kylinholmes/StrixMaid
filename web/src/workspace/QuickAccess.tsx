import { useQuery } from "@tanstack/react-query";
import { HardDrive, Home, Slash } from "lucide-react";
import { api } from "@/api/client";
import type { components } from "@/api/schema";
import { capabilitiesQuery } from "@/app/queries";
import { cx } from "@/lib/cx";
import { fmtBytes } from "@/lib/fmt";
import { useSession } from "@/session/useSession";
import { folderIconUrl } from "./icons";
import { guessHome, joinPath, type Platform, platformOf } from "./path";
import { type IconSubject, useEntryIcon } from "./sysicons";
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

/** 网络文件系统：无论挂在哪都值得展示。 */
function isNetworkFs(fsType: string): boolean {
  return ["nfs", "nfs4", "cifs", "smbfs", "smb3", "afpfs", "webdav"].includes(fsType.toLowerCase());
}

/**
 * 这个挂载点值不值得占快速访问一行（负责人反馈：mac 上一屏
 * /System/Volumes/* 根本看不懂）。根永远要；网络盘永远要；外置/可移动
 * 介质的常见挂载区要；系统自身的簿记卷全部藏掉。
 */
function mountWorthShowing(f: FilesystemInfo): boolean {
  const mp = f.mount_point;
  if (mp === "/" || /^[A-Za-z]:\\$/.test(mp)) return true;
  if (isNetworkFs(f.fs_type)) return true;
  const good = ["/Volumes/", "/mnt/", "/media/", "/run/media/"];
  if (good.some((p) => mp.startsWith(p))) return true;
  return false;
}

/** 挂载点的展示名：根叫系统盘，其余取末段。完整路径在 title 里。 */
function mountLabel(f: FilesystemInfo): string {
  const mp = f.mount_point;
  if (mp === "/") return "系统盘";
  if (/^[A-Za-z]:\\$/.test(mp)) return mp;
  const seg = mp
    .replace(/[\\/]+$/, "")
    .split(/[\\/]/)
    .pop();
  return seg || mp;
}

/**
 * 会话生命周期内见过的挂载点（§4.2 的「无响应」判定基线）。
 *
 * **模块级**而不是组件内的 ref：快速访问栏折叠、或切去别的页面时组件会
 * 卸载，记忆跟着组件走的话，回来那一刻基线清零，卡死的挂载正好「凭空
 * 消失」——恰是这条需求要防的事。
 */
const seenMounts = new Map<string, FilesystemInfo>();

/**
 * 左栏条目的图标：系统真身优先（mac 上主目录/桌面/下载有带徽标的专属图标、
 * 挂载点是磁盘的样子），取不到回落调用方给的形状。判定统一走
 * `sysicons.ts` 的 [`iconKeysOf`]（经 [`useEntryIcon`]），本组件不自设条件。
 *
 * **不给无响应的挂载点用**：按路径取图标会真的碰那个路径，对卡死的挂载
 * 就是又一次挂起的调用——stale 条目由调用方直接给形状，不进这里。
 */
function RailIcon({
  subject,
  platform,
  fallback,
}: {
  subject: IconSubject;
  platform: Platform;
  fallback: React.ReactNode;
}) {
  const sys = useEntryIcon(subject, platform);
  return sys ? <img src={sys} width={14} height={14} alt="" /> : fallback;
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
/** 已知文件夹候选：物理目录名 → 展示名（中文物理名的系统直接同名命中）。 */
const KNOWN_FOLDERS: readonly (readonly [string, string])[] = [
  ["Desktop", "桌面"],
  ["桌面", "桌面"],
  ["Documents", "文稿"],
  ["文档", "文档"],
  ["Downloads", "下载"],
  ["下载", "下载"],
  ["Pictures", "图片"],
  ["图片", "图片"],
  ["Music", "音乐"],
  ["音乐", "音乐"],
  ["Movies", "影片"],
  ["Videos", "视频"],
  ["视频", "视频"],
  ["Public", "公共"],
];

export function QuickAccess({ current, onGo }: QuickAccessProps) {
  const caps = useQuery(capabilitiesQuery());
  const user = useSession((st) => st.user);
  const fs = useQuery(filesystemsQuery());

  for (const f of fs.data ?? []) seenMounts.set(f.mount_point, f);
  const liveMounts = (fs.data ?? []).filter((f) => mountWorthShowing(f));
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

  // 已知文件夹按主目录**实际存在的**列（探测而不是硬编码：mac 是英文物理名、
  // 中文 Linux 常是中文物理名，列出来的一定点得进去）。与地址栏补全共用缓存键。
  const homeList = useQuery({
    queryKey: ["complete", home],
    enabled: home !== null,
    staleTime: 60_000,
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/files", {
        params: { query: { path: home as string, limit: 200, sort: "name", order: "asc" } },
      });
      if (error) throw error;
      return data;
    },
  });
  const knownFolders =
    home === null
      ? []
      : KNOWN_FOLDERS.flatMap(([name, label]) => {
          const hit = (homeList.data?.entries ?? []).find(
            (e) => e.kind === "dir" && e.name === name,
          );
          return hit ? [{ path: joinPath(home, name, platform), name, label }] : [];
        });

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
        {home &&
          item(
            home,
            "主目录",
            <RailIcon
              subject={{ kind: "home", fullPath: home }}
              platform={platform}
              fallback={<Home size={14} strokeWidth={1.5} />}
            />,
          )}
        {knownFolders.map((kf) =>
          item(
            kf.path,
            kf.label,
            <RailIcon
              subject={{ kind: "known-folder", fullPath: kf.path }}
              platform={platform}
              fallback={<img src={folderIconUrl(kf.name)} width={14} height={14} alt="" />}
            />,
          ),
        )}
        {item(
          root,
          platform === "windows" ? "全部驱动器" : "根目录",
          // Windows 的 `\` 是虚拟根不是路径，RailIcon 的 pathKey 在
          // windows 平台本来就为 null，回落 Slash。
          <RailIcon
            subject={{ kind: "mount", fullPath: root }}
            platform={platform}
            fallback={<Slash size={14} strokeWidth={1.5} />}
          />,
        )}
      </div>
      <div className={s.railGroup}>
        <span className={s.railLabel}>{platform === "windows" ? "驱动器" : "挂载点"}</span>
        {liveMounts.map((f) => (
          <div key={f.mount_point}>
            <button
              type="button"
              className={cx(s.railItem, current === f.mount_point && s.railItemActive)}
              onClick={() => onGo(f.mount_point)}
              title={f.mount_point}
            >
              {/* 网络盘不按路径取图标：那是一次真实的路径访问，NAS 抖一下
                  就挂起一个后端阻塞线程；statvfs 活着不代表图标读得动。 */}
              {isNetworkFs(f.fs_type) ? (
                <HardDrive size={14} strokeWidth={1.5} />
              ) : (
                <RailIcon
                  subject={{ kind: "mount", fullPath: f.mount_point }}
                  platform={platform}
                  fallback={<HardDrive size={14} strokeWidth={1.5} />}
                />
              )}
              <span className={s.mountLines}>
                <span className={s.railItemName}>
                  {mountLabel(f)}
                  {isNetworkFs(f.fs_type) && <span className={s.staleTag}>（网络）</span>}
                </span>
                <span className={s.mountSub}>
                  {fmtBytes(f.available_bytes)} 可用 · {f.fs_type}
                </span>
              </span>
            </button>
            <div className={s.railUsage} aria-hidden="true">
              <i
                style={{
                  width: `${Math.min(100, (f.used_bytes / Math.max(1, f.total_bytes)) * 100)}%`,
                }}
              />
            </div>
          </div>
        ))}
        {staleMounts
          .filter(mountWorthShowing)
          .map((f) =>
            item(
              f.mount_point,
              mountLabel(f),
              <HardDrive size={14} strokeWidth={1.5} />,
              <span className={s.staleTag}>无响应</span>,
              true,
            ),
          )}
      </div>
    </nav>
  );
}
