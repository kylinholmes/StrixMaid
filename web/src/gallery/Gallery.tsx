import {
  Activity,
  Files,
  FileText,
  Gauge,
  LayoutGrid,
  ListTree,
  Server,
  Settings,
  ShieldCheck,
  TerminalSquare,
} from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";
import {
  Button,
  type Column,
  Dialog,
  DistroMark,
  EmptyState,
  ErrorState,
  Field,
  KeyValueGrid,
  Menu,
  Meter,
  type NavItem,
  NavRail,
  ProgressLine,
  SearchBox,
  Segmented,
  Sparkline,
  StatusDot,
  Table,
  TableSkeleton,
  Tag,
  Toolbar,
  ToolbarSpacer,
} from "@/components";
import { DISTROS, UNKNOWN_DISTRO } from "@/theme/distro";
import { RAMP } from "@/theme/tokens";
import { useTheme } from "@/theme/useTheme";
import { SERVICES, type Service, series } from "./data";
import s from "./Gallery.module.css";

const NAV: readonly NavItem[] = [
  { id: "overview", label: "概览", icon: LayoutGrid },
  { id: "perf", label: "性能", icon: Activity },
  { id: "services", label: "服务", icon: Server, count: "2", countTone: "bad" },
  { id: "logs", label: "日志", icon: FileText },
  { id: "processes", label: "进程", icon: ListTree },
  { id: "terminal", label: "终端", icon: TerminalSquare },
  { id: "files", label: "文件", icon: Files },
  { id: "audit", label: "审计", icon: ShieldCheck, disabled: true },
  { id: "nodes", label: "节点", icon: Gauge, count: "1" },
  { id: "settings", label: "设置", icon: Settings },
];

const COLUMNS: readonly Column<Service>[] = [
  { key: "name", header: "服务", mono: true, render: (r) => r.name },
  { key: "state", header: "状态", render: (r) => <StatusDot state={r.state} /> },
  { key: "enabled", header: "启用", dim: true, render: (r) => r.enabled },
  { key: "source", header: "来源", render: (r) => <Tag>{r.source}</Tag> },
  { key: "desc", header: "描述", dim: true, width: "34%", render: (r) => r.desc },
  { key: "cpu", header: "CPU %", numeric: true, render: (r) => r.cpu },
  { key: "mem", header: "内存", numeric: true, render: (r) => r.mem },
];

function Section({
  n,
  title,
  note,
  children,
}: {
  n: string;
  title: string;
  note?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className={s.sec}>
      <div className={s.secHead}>
        <span className={s.secNum}>{n}</span>
        <h2>{title}</h2>
      </div>
      {note && <p className={s.secNote}>{note}</p>}
      {children}
    </section>
  );
}

export function Gallery() {
  const { mode, distro, setMode, setDistroById } = useTheme();
  const [selected, setSelected] = useState<string | null>("containerd.service");
  const [query, setQuery] = useState("");
  const [dialog, setDialog] = useState(false);
  const [narrow, setNarrow] = useState(false);

  // 合集页默认展示 Ubuntu：派生正是这一页要看的东西，一上来给纯灰等于什么都没展示。
  // **只在挂载时跑一次**——不加这个守卫的话，用户永远选不中「认不出」，
  // 因为每次切过去都会被这个 effect 立刻扳回来。
  const bootstrapped = useRef(false);
  useEffect(() => {
    if (bootstrapped.current) return;
    bootstrapped.current = true;
    if (distro.id === UNKNOWN_DISTRO.id) setDistroById("ubuntu");
  }, [distro.id, setDistroById]);

  const ramp = RAMP[mode];

  return (
    <div className={s.page}>
      <div className={s.bar}>
        <div className={s.ctl}>
          <span>发行版</span>
          <Segmented
            label="发行版"
            value={distro.id}
            onChange={(id) => setDistroById(id === "unknown" ? null : id)}
            options={[
              ...DISTROS.slice(0, 6).map((d) => ({ value: d.id, label: d.name })),
              { value: UNKNOWN_DISTRO.id, label: "认不出" },
            ]}
          />
        </div>
        <div className={s.ctl}>
          <span>主题</span>
          <Segmented
            label="主题"
            value={mode}
            onChange={setMode}
            options={[
              { value: "dark", label: "暗" },
              { value: "light", label: "亮" },
            ]}
          />
        </div>
        <div className={s.ctl}>
          <span>左栏</span>
          <Segmented
            label="左栏宽度"
            value={narrow ? "narrow" : "full"}
            onChange={(v) => setNarrow(v === "narrow")}
            options={[
              { value: "full", label: "完整 184px" },
              { value: "narrow", label: "图标条 44px" },
            ]}
          />
        </div>
      </div>

      <div className={s.wrap}>
        <p className={s.lede}>
          全部组件按{" "}
          <code>docs/superpowers/specs/2026-08-29-frontend-design-language-design.md</code>{" "}
          实现。中性色由发行版主色派生——
          <b>切上面的发行版，整页的灰会跟着换色温，但数据色一动不动</b>。 切到「认不出」会回落纯灰。
        </p>

        <Section
          n="01"
          title="色彩"
          note={
            <>
              上排是中性色阶（由发行版色相派生，<b>暗色 C 0.005 / 亮色 C 0.004</b>
              ），下排是数据色（静态， 穷举验证过色盲安全）。<b>一个像素只能属于一层</b>。
            </>
          }
        >
          <div className={`${s.panel} ${s.pad}`}>
            <div className={s.ramp}>
              {Object.entries(ramp)
                .filter(([k]) => k !== "sel")
                .map(([k, l]) => (
                  <div key={k} style={{ background: `var(--${k})` }}>
                    <span style={{ color: l > 55 ? "#111" : "#eee" }}>{k}</span>
                  </div>
                ))}
            </div>
            <div className={s.swatches}>
              {[
                ["--cpu", "CPU"],
                ["--gpu", "GPU"],
                ["--mem", "内存"],
                ["--disk", "磁盘"],
                ["--net", "网络"],
                ["--ok", "正常"],
                ["--warn", "警告"],
                ["--crit", "严重"],
              ].map(([v, label]) => (
                <span className={s.sw} key={v}>
                  <i style={{ background: `var(${v})` }} />
                  {label}
                </span>
              ))}
            </div>
          </div>
        </Section>

        <Section
          n="02"
          title="按钮与输入"
          note={
            <>
              普通按钮<b>没有品牌色</b>
              。红色实心是状态色进入界面层的唯一例外，只允许用于破坏性操作， 且必须配二次确认。
            </>
          }
        >
          <div className={s.grid2}>
            <div className={`${s.panel} ${s.pad} ${s.col}`}>
              <div className={s.row}>
                <Button variant="primary">启动</Button>
                <Button>停止</Button>
                <Button disabled>重启</Button>
                <Button variant="danger" onClick={() => setDialog(true)}>
                  强制终止
                </Button>
              </div>
              <div className={s.row}>
                <Button size="sm" variant="primary">
                  小号主
                </Button>
                <Button size="sm">小号次</Button>
                <Segmented
                  label="演示"
                  value="a"
                  onChange={() => {}}
                  options={[
                    { value: "a", label: "系统" },
                    { value: "b", label: "用户" },
                  ]}
                />
              </div>
            </div>
            <div className={`${s.panel} ${s.pad} ${s.col}`}>
              <Field label="用户名" placeholder="alice" defaultValue="alice" />
              <Field
                label="密码"
                type="password"
                error="用户名或密码不正确。连续失败会被系统的认证策略延迟。"
              />
            </div>
          </div>
        </Section>

        <Section
          n="03"
          title="标记"
          note={<>颜色从来不是唯一的身份编码——每个状态标记旁必有文字，发行版方块带首字母。</>}
        >
          <div className={`${s.panel} ${s.pad} ${s.col}`}>
            <div className={s.row}>
              <StatusDot state="run" />
              <StatusDot state="stop" />
              <StatusDot state="fail" />
              <StatusDot state="unknown" />
            </div>
            <div className={s.row}>
              <Tag>systemd</Tag>
              <Tag tone="warn">需重启</Tag>
              <Tag tone="bad">2 失败</Tag>
              <Meter value={0.66} label="根分区使用率" tone="--disk" />
              <Meter value={0.91} label="日志分区使用率" tone="--crit" />
            </div>
            <div className={s.row}>
              {[...DISTROS.slice(0, 6), UNKNOWN_DISTRO].map((d) => (
                <span key={d.id} className={s.sw}>
                  <DistroMark distro={d} />
                  {d.name}
                </span>
              ))}
            </div>
            <div className={s.row}>
              <Sparkline data={series(11, 40, 14, 9)} tone="--cpu" max={100} label="CPU 占用" />
              <Sparkline data={series(33, 40, 60, 4)} tone="--mem" max={100} label="内存占用" />
              <Sparkline data={series(44, 40, 40, 22)} tone="--disk" max={100} label="磁盘繁忙" />
              <Sparkline data={series(55, 40, 30, 26)} tone="--net" label="网络吞吐" />
            </div>
          </div>
        </Section>

        <Section
          n="04"
          title="表格"
          note={
            <>
              斑马纹，不画行线。四级明度阶梯：奇数行 → 偶数行 → 划过 → 选中。
              <b>奇偶只差 4%，是全套语言里最紧的一处</b>，请划过几行确认分得开。
            </>
          }
        >
          <div className={s.panel}>
            <Toolbar>
              <SearchBox
                value={query}
                onChange={setQuery}
                label="搜索服务"
                placeholder="搜索服务"
              />
              <Tag>全部 312</Tag>
              <Tag>运行 289</Tag>
              <Tag tone="bad">失败 2</Tag>
              <ToolbarSpacer />
              <Button disabled>停止</Button>
              <Button disabled>重启</Button>
              <Button variant="primary">启动</Button>
            </Toolbar>
            <Table
              caption="服务列表"
              columns={COLUMNS}
              rows={SERVICES.filter((x) => x.name.includes(query))}
              rowKey={(r) => r.name}
              selectedKey={selected}
              onSelect={(r) => setSelected(r.name)}
              empty={
                <EmptyState
                  title={`没有匹配「${query}」的服务。`}
                  detail="这台机器上共有 312 个服务。"
                  actionLabel="清除搜索"
                  onAction={() => setQuery("")}
                />
              }
            />
          </div>
        </Section>

        <Section
          n="05"
          title="浮层"
          note={
            <>
              双层阴影 + 1px 边框。对话框加全屏遮罩，下拉不加。暗色下阴影能看见，靠的是底提到了
              L22.4。
            </>
          }
        >
          <div className={s.grid2}>
            <div className={s.panel}>
              <div className={s.label}>下拉菜单</div>
              <div className={s.menuHost}>
                {["nginx.service　运行中", "containerd.service　失败", "sshd.service　运行中"].map(
                  (t) => (
                    <div className={s.menuBg} key={t}>
                      {t}
                    </div>
                  ),
                )}
                <Menu
                  label="服务操作"
                  style={{ left: 120, top: 40 }}
                  onPick={() => {}}
                  items={[
                    { id: "start", label: "启动" },
                    { id: "stop", label: "停止", disabled: true },
                    { id: "restart", label: "重启", disabled: true },
                    { id: "enable", label: "开机自启" },
                    { id: "mask", label: "屏蔽", destructive: true },
                  ]}
                />
              </div>
            </div>
            <div className={`${s.panel} ${s.pad} ${s.col}`}>
              <p className={s.secNote} style={{ margin: 0 }}>
                破坏性操作必须弹二次确认，框里说清后果——哪个信号、会丢什么，不是「确定吗」。
              </p>
              <div>
                <Button variant="danger" onClick={() => setDialog(true)}>
                  打开确认框
                </Button>
              </div>
              <KeyValueGrid
                items={[
                  { k: "服务", v: "containerd.service", mono: true },
                  { k: "状态", v: "失败 · exit-code 1" },
                  { k: "来源", v: "systemd", mono: true },
                  { k: "上次启动", v: "3 分钟前" },
                ]}
              />
            </div>
          </div>
        </Section>

        <Section
          n="06"
          title="加载 · 空 · 错误"
          note={
            <>
              骨架屏<b>只用于首次进入</b>；刷新已有数据时保留数据、只走顶部那条 2px 进度线，
              否则每次轮询整表闪一下。
            </>
          }
        >
          <div className={s.grid3}>
            <div className={s.panel}>
              <div className={s.label}>首次进入</div>
              <TableSkeleton rows={5} />
            </div>
            <div className={s.panel}>
              <div className={s.label}>刷新（已有数据）</div>
              <ProgressLine />
              <div style={{ opacity: 0.72 }}>
                <Table
                  caption="刷新中的服务列表"
                  columns={COLUMNS.slice(0, 3)}
                  rows={SERVICES.slice(0, 4)}
                  rowKey={(r) => r.name}
                />
              </div>
            </div>
            <div className={s.panel}>
              <div className={s.label}>出错</div>
              <ErrorState
                title="读取服务列表失败。"
                detail="与服务管理器的连接被拒绝，可能是它尚未启动。"
                command="systemctl status dbus.service"
                onRetry={() => {}}
              />
            </div>
          </div>
        </Section>

        <Section
          n="07"
          title="概念页 · 服务"
          note={
            <>
              把上面的组件合起来。注意两处「无权」的表现：<b>停止 / 重启是禁用而不是隐藏</b>
              （有能力、当前用户没权限）；而「审计」在左栏是禁用态。如果这台机器探测不到服务管理器，
              <b>「服务」这一项根本不会出现在左栏</b>——不是灰掉。
            </>
          }
        >
          <div className={s.mock}>
            <NavRail items={NAV} current="services" onNavigate={() => {}} narrow={narrow} />
            <div className={s.mockMain}>
              <div className={s.mockTop}>
                <DistroMark distro={distro} />
                <b>web-01.lan</b>
                <span className={s.os}>
                  {distro.brand ? `${distro.name} 24.04.1 LTS` : "未识别的 Linux 发行版"}
                </span>
                <ToolbarSpacer />
                <span className={s.os}>alice</span>
                <Tag>未提权</Tag>
              </div>
              <Toolbar>
                <SearchBox value="" onChange={() => {}} label="搜索服务" placeholder="搜索服务" />
                <Tag>系统</Tag>
                <Tag tone="bad">失败 2</Tag>
                <ToolbarSpacer />
                <Button disabled>停止</Button>
                <Button disabled>重启</Button>
                <Button variant="primary">启动</Button>
              </Toolbar>
              <div className={s.mockBody}>
                <Table
                  caption="服务列表"
                  columns={COLUMNS}
                  rows={SERVICES}
                  rowKey={(r) => r.name}
                  selectedKey={selected}
                  onSelect={(r) => setSelected(r.name)}
                />
              </div>
              <div className={s.mockDetail}>
                <KeyValueGrid
                  items={[
                    { k: "服务", v: selected ?? "—", mono: true },
                    { k: "状态", v: <StatusDot state="fail" label="失败 · exit-code 1" /> },
                    { k: "来源", v: "systemd", mono: true },
                    { k: "开机", v: "开机自启" },
                    { k: "上次启动", v: "3 分钟前" },
                    { k: "主进程", v: "—", mono: true },
                  ]}
                />
              </div>
            </div>
          </div>
        </Section>
      </div>

      <Dialog
        open={dialog}
        destructive
        title="强制终止 containerd？"
        confirmLabel="强制终止"
        onConfirm={() => setDialog(false)}
        onCancel={() => setDialog(false)}
      >
        将向 <b>containerd.service</b> 的主进程发送 <b>SIGKILL</b>。
        进程不会做任何清理，未落盘的数据会丢失。
      </Dialog>
    </div>
  );
}
