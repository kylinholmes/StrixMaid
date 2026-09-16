import { useQuery } from "@tanstack/react-query";
import { X } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router-dom";
import { api } from "@/api/client";
import {
  Button,
  Dialog,
  ErrorState,
  KeyValueGrid,
  type KeyValueItem,
  StatusDot,
} from "@/components";
import { cx } from "@/lib/cx";
import { fmtBytes } from "@/lib/fmt";
import s from "./Svc.module.css";
import { enableLabel } from "./UnitTable";
import type { Scope } from "./useUnits";

type UnitAction =
  | "start"
  | "stop"
  | "restart"
  | "reload"
  | "enable"
  | "disable"
  | "mask"
  | "unmask";

/** 启用方式下拉能选中的三个动作。unmask 不在其中:它是切换时自动补的前置步骤。 */
type EnableChoice = "enable" | "disable" | "mask";

/** 会走确认弹层的动作。start 不打断任何东西,unmask 不由用户直接发起。 */
type Confirmable = Exclude<UnitAction, "start" | "unmask">;

/**
 * 启用方式的三个取值,照 Windows 服务管理器的「启动类型」做成下拉而不是两个开关:
 * 三者互斥,下拉一眼看得出当前是哪一个。
 *
 * 动作在两个平台上分别落到:
 * enable → `SERVICE_AUTO_START` / `systemctl enable`,
 * disable → `SERVICE_DEMAND_START` / `systemctl disable`,
 * mask → `SERVICE_DISABLED` / `systemctl mask`。
 *
 * 文案取自 UnitTable 的 `enableLabel`,同一个状态在列表和详情里叫法一致。
 */
const ENABLE_CHOICES: readonly { action: EnableChoice; label: string }[] = [
  { action: "enable", label: "自启" },
  { action: "disable", label: "手动" },
  { action: "mask", label: "已屏蔽" },
];

/**
 * 由 unit 文件自身决定、enable/disable/mask 都改不动的启用态。
 * 选中它们里的任何一个都只会换来一条错误,所以下拉整体禁用,只如实显示当前值。
 * `unknown` 一并算在内:后端没认出这个取值,前端更没有依据断定它可改。
 */
const FIXED_STATES: ReadonlySet<string> = new Set([
  "static",
  "indirect",
  "alias",
  "generated",
  "transient",
  "unknown",
]);

/** 启用态 → 下拉里对应的选项。只有持久化的那三个对得上,其余(含 runtime 变体)没有。 */
function choiceOf(state: string | null | undefined): EnableChoice | null {
  switch (state) {
    case "enabled":
      return "enable";
    case "disabled":
      return "disable";
    case "masked":
      return "mask";
    default:
      return null;
  }
}

/** 当前启用态对不上三个选项时,占位项的取值。与任何 UnitAction 都不重名。 */
const AS_IS = "as-is";

/** 确认弹层文案:写清后果,不写「确定吗」(spec §5.1)。 */
const CONFIRM: Record<Confirmable, { label: string; consequence: string; destructive: boolean }> = {
  stop: {
    label: "停止",
    consequence:
      "停止该 unit。依赖它的 unit 可能一并停止;socket 激活的服务可能随下一次连接再被拉起。",
    destructive: true,
  },
  restart: {
    label: "重启",
    consequence: "先停后起,期间服务短暂不可用。",
    destructive: true,
  },
  reload: {
    label: "重载",
    consequence: "让 unit 重载配置。未声明 ExecReload 的 unit 会直接失败,不会退化成重启。",
    destructive: false,
  },
  enable: {
    label: "设为自启",
    consequence: "系统启动时自动拉起。不影响当前运行状态。",
    destructive: false,
  },
  disable: {
    label: "设为手动",
    consequence: "下次开机不再自动拉起,只在被显式启动或被依赖拉起时运行。不影响当前运行状态。",
    destructive: false,
  },
  mask: {
    label: "屏蔽",
    consequence: "此后任何启动尝试——包括其他 unit 的依赖拉起——都会失败,直到改回其他启用方式。",
    destructive: true,
  },
};

function ts(v: number | null | undefined): string | null {
  if (v === null || v === undefined) return null;
  return new Date(v * 1000).toLocaleString("zh-CN", { hour12: false });
}

export function UnitDetail({
  unit,
  scope,
  onClose,
}: {
  unit: string;
  scope: Scope;
  onClose: () => void;
}) {
  const [pending, setPending] = useState<Confirmable | null>(null);
  const [actionErr, setActionErr] = useState<string | null>(null);
  const [actionNote, setActionNote] = useState<string | null>(null);
  const [depsOpen, setDepsOpen] = useState(false);
  const [fileOpen, setFileOpen] = useState(false);
  const [logsOpen, setLogsOpen] = useState(true);

  const detail = useQuery({
    queryKey: ["svc", "unit", scope, unit],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/services/{unit}", {
        params: { path: { unit }, query: { scope } },
      });
      if (error) throw error;
      return data;
    },
    refetchInterval: 3_000,
    retry: 0,
  });

  const deps = useQuery({
    queryKey: ["svc", "deps", scope, unit],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/services/{unit}/deps", {
        params: { path: { unit }, query: { scope } },
      });
      if (error) throw error;
      return data;
    },
    enabled: depsOpen,
    retry: 0,
    staleTime: 30_000,
  });

  const file = useQuery({
    queryKey: ["svc", "file", scope, unit],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/services/{unit}/file", {
        params: { path: { unit }, query: { scope } },
      });
      if (error) throw error;
      return data;
    },
    enabled: fileOpen,
    retry: 0,
    staleTime: 30_000,
  });

  // 服务 → 它的日志:排障的主路径。unit 过滤在两个平台都成立
  // (Linux journald -u;macOS 后端把 unit 映射到统一日志 subsystem)。
  const recentLogs = useQuery({
    queryKey: ["svc", "logs", scope, unit],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/logs", {
        params: { query: { unit, limit: 15 } },
      });
      if (error) throw error;
      return data;
    },
    enabled: logsOpen,
    refetchInterval: 10_000,
    retry: 0,
  });

  const d = detail.data;
  const masked = d?.enable_state === "masked" || d?.enable_state === "masked_runtime";

  const act = async (action: UnitAction) => {
    setActionErr(null);
    setActionNote(null);
    // 已屏蔽的 unit 要先解除屏蔽,enable/disable 才落得下去:systemd 会直接拒掉
    // 对 masked unit 的 enable,SCM 那边 unmask 也只是把 DISABLED 抬回 DEMAND_START。
    // 只对这两个动作补,不对 start/stop 补——顺手解屏蔽是用户没要求的副作用。
    const steps: UnitAction[] =
      masked && (action === "enable" || action === "disable") ? ["unmask", action] : [action];
    for (const step of steps) {
      const { error } = await api.POST("/api/v1/services/{unit}/action", {
        params: { path: { unit }, query: { scope } },
        body: { action: step },
      });
      if (error) {
        const e = error as { message?: string; detail?: string };
        setActionErr(e.message ?? "操作失败");
        return;
      }
    }
    setActionNote(`${action} 已提交,状态以下方实时读数为准`);
    void detail.refetch();
  };

  const dot =
    d?.active_state === "active"
      ? "run"
      : d?.active_state === "failed"
        ? "fail"
        : d?.active_state === "inactive"
          ? "stop"
          : "unknown";

  // 启用方式:对得上三个持久化取值就选中对应项;对不上的(runtime 变体、linked)
  // 另开一个占位项如实显示当前值,不并进「自启」「已屏蔽」——它们只在本次启动内有效。
  const enableState = d?.enable_state ?? null;
  const choice = choiceOf(enableState);
  const enableFixed = enableState === null || FIXED_STATES.has(enableState);

  const facts: KeyValueItem[] = [];
  if (d) {
    if (d.description !== d.name) facts.push({ k: "描述", v: d.description });
    facts.push({
      k: "启用",
      v: (
        <select
          className={s.enableSel}
          aria-label="启用方式"
          title={enableState ?? undefined}
          value={choice ?? AS_IS}
          disabled={enableFixed}
          onChange={(e) => setPending(e.target.value as EnableChoice)}
        >
          {choice === null && (
            <option value={AS_IS} disabled>
              {enableLabel(d).text}
            </option>
          )}
          {ENABLE_CHOICES.map((c) => (
            <option key={c.action} value={c.action}>
              {c.label}
            </option>
          ))}
        </select>
      ),
    });
    const enter = ts(d.active_enter_ts);
    if (enter) facts.push({ k: "进入当前状态", v: enter });
    if (d.n_restarts !== undefined && d.n_restarts !== null && d.n_restarts > 0)
      facts.push({ k: "重启次数", v: String(d.n_restarts) });
    if (d.result && d.result !== "success")
      facts.push({
        k: "上次结果",
        v: `${d.result}${d.exit_code ? `（退出码 ${d.exit_code}）` : ""}`,
      });
    if (d.main_pid !== undefined && d.main_pid !== null)
      facts.push({ k: "主进程", v: `PID ${d.main_pid}`, mono: true });
    if (d.user) facts.push({ k: "运行身份", v: d.user, mono: true });
    if (d.fragment_path) facts.push({ k: "unit 文件", v: d.fragment_path, mono: true });
    if (d.cgroup?.path) facts.push({ k: "cgroup", v: d.cgroup.path, mono: true });
  }
  const cg = d?.cgroup;

  return (
    <aside className={s.drawer} aria-label={`unit ${unit} 详情`}>
      <div className={s.drawerHead}>
        <b className={s.drawerTitle}>{unit}</b>
        <span className={s.drawerSub}>{scope === "user" ? "用户" : "系统"}</span>
        <Button size="sm" iconOnly aria-label="关闭详情" onClick={onClose}>
          <X size={14} strokeWidth={1.5} />
        </Button>
      </div>

      {detail.isError ? (
        <div className={s.drawerBody}>
          <ErrorState
            title="拿不到 unit 详情"
            detail={(detail.error as { message?: string }).message}
          />
        </div>
      ) : (
        <div className={s.drawerBody}>
          {d && (
            <>
              <div className={s.drawerStats}>
                <div>
                  <div className={s.k}>状态</div>
                  <div className={s.v}>
                    <StatusDot state={dot} label={d.sub_state} />
                  </div>
                </div>
                {cg?.cpu_percent !== undefined && cg?.cpu_percent !== null && (
                  <div>
                    <div className={s.k}>CPU</div>
                    <div className={s.v}>{cg.cpu_percent.toFixed(1)}%</div>
                  </div>
                )}
                {cg?.memory_current_bytes !== undefined && cg?.memory_current_bytes !== null && (
                  <div>
                    <div className={s.k}>内存</div>
                    <div className={s.v}>
                      {fmtBytes(cg.memory_current_bytes)}
                      {cg.memory_limit_bytes !== undefined && cg.memory_limit_bytes !== null && (
                        <small> / {fmtBytes(cg.memory_limit_bytes)}</small>
                      )}
                    </div>
                  </div>
                )}
                {cg?.tasks_current !== undefined && cg?.tasks_current !== null && (
                  <div>
                    <div className={s.k}>任务</div>
                    <div className={s.v}>
                      {cg.tasks_current}
                      {cg.tasks_limit !== undefined && cg.tasks_limit !== null && (
                        <small> / {cg.tasks_limit}</small>
                      )}
                    </div>
                  </div>
                )}
              </div>

              <KeyValueGrid items={facts} columns={1} />

              <details
                className={s.fold}
                open
                onToggle={(e) => setLogsOpen((e.target as HTMLDetailsElement).open)}
              >
                <summary>最近日志</summary>
                <div className={s.foldBody}>
                  {recentLogs.isError && (
                    <p className={s.actionNote}>
                      {(recentLogs.error as { message?: string }).message ?? "拿不到日志"}
                    </p>
                  )}
                  {(recentLogs.data?.entries ?? []).map((e) => (
                    <div className={s.logLine} key={e.cursor}>
                      <span className={s.logTime}>
                        {new Date(e.ts * 1000).toLocaleTimeString("zh-CN", { hour12: false })}
                      </span>
                      <span
                        className={cx(
                          s.logPrio,
                          e.priority === "warning" && s.logPrioWarn,
                          (e.priority === "err" ||
                            e.priority === "crit" ||
                            e.priority === "alert" ||
                            e.priority === "emerg") &&
                            s.logPrioCrit,
                        )}
                      >
                        {e.priority.toUpperCase()}
                      </span>
                      <span className={s.logMsg} title={e.message}>
                        {e.message}
                      </span>
                    </div>
                  ))}
                  {recentLogs.data && (recentLogs.data.entries ?? []).length === 0 && (
                    <p className={s.actionNote}>窗口内没有这个 unit 的日志。</p>
                  )}
                  <Link
                    className={s.logAll}
                    to={`/logs?unit=${encodeURIComponent(unit)}&priority=all`}
                  >
                    在日志页查看全部 →
                  </Link>
                </div>
              </details>

              <details
                className={s.fold}
                onToggle={(e) => setDepsOpen((e.target as HTMLDetailsElement).open)}
              >
                <summary>依赖关系</summary>
                <div className={s.foldBody}>
                  {deps.isError && (
                    <p className={s.actionNote}>
                      {(deps.error as { message?: string }).message ?? "拿不到依赖"}
                    </p>
                  )}
                  {deps.data &&
                    (
                      [
                        ["依赖（Requires）", deps.data.requires],
                        ["希望（Wants）", deps.data.wants],
                        ["被依赖", deps.data.required_by],
                        ["被希望", deps.data.wanted_by],
                        ["之后启动（After）", deps.data.after],
                        ["触发", deps.data.triggers],
                        ["被触发", deps.data.triggered_by],
                      ] as const
                    )
                      .filter(([, v]) => v.length > 0)
                      .map(([k, v]) => (
                        <div className={s.depGroup} key={k}>
                          <div className={s.k}>{k}</div>
                          <div className={s.v}>{v.join("  ")}</div>
                        </div>
                      ))}
                  {deps.data &&
                    [
                      deps.data.requires,
                      deps.data.wants,
                      deps.data.required_by,
                      deps.data.wanted_by,
                      deps.data.after,
                      deps.data.triggers,
                      deps.data.triggered_by,
                    ].every((v) => v.length === 0) && (
                      <p className={s.actionNote}>没有声明依赖。</p>
                    )}
                </div>
              </details>

              <details
                className={s.fold}
                onToggle={(e) => setFileOpen((e.target as HTMLDetailsElement).open)}
              >
                <summary>unit 文件（只读）</summary>
                <div className={s.foldBody}>
                  {file.isError && (
                    <p className={s.actionNote}>
                      {(file.error as { message?: string }).message ?? "拿不到 unit 文件"}
                    </p>
                  )}
                  {file.data?.fragment && (
                    <>
                      <div className={s.filePath}>{file.data.fragment.path}</div>
                      <pre className={s.fileBody}>{file.data.fragment.content}</pre>
                    </>
                  )}
                  {(file.data?.drop_ins ?? []).map((f) => (
                    <div key={f.path}>
                      <div className={s.filePath}>{f.path}（drop-in）</div>
                      <pre className={s.fileBody}>{f.content}</pre>
                    </div>
                  ))}
                  {file.data && !file.data.fragment && (file.data.drop_ins ?? []).length === 0 && (
                    <p className={s.actionNote}>transient unit,磁盘上没有文件。</p>
                  )}
                </div>
              </details>

              <div className={s.actions}>
                <div className={s.actionRow}>
                  <Button size="sm" onClick={() => void act("start")}>
                    启动
                  </Button>
                  <Button size="sm" variant="secondary" onClick={() => setPending("stop")}>
                    停止
                  </Button>
                  <Button size="sm" variant="secondary" onClick={() => setPending("restart")}>
                    重启
                  </Button>
                  <Button size="sm" variant="secondary" onClick={() => setPending("reload")}>
                    重载
                  </Button>
                </div>
                {actionErr && <p className={s.actionErr}>{actionErr}</p>}
                {actionNote && <p className={s.actionNote}>{actionNote}</p>}
              </div>
            </>
          )}
        </div>
      )}

      <Dialog
        open={pending !== null}
        title={pending ? `${CONFIRM[pending].label} ${unit}` : ""}
        confirmLabel={pending ? CONFIRM[pending].label : ""}
        destructive={pending ? CONFIRM[pending].destructive : false}
        onCancel={() => setPending(null)}
        onConfirm={() => {
          if (pending) void act(pending);
          setPending(null);
        }}
      >
        {pending ? CONFIRM[pending].consequence : null}
      </Dialog>
    </aside>
  );
}
