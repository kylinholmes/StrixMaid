import { useQuery } from "@tanstack/react-query";
import { X } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router-dom";
import { api } from "@/api/client";
import { Button, Dialog, ErrorState, KeyValueGrid, type KeyValueItem } from "@/components";
import { fmtBytes } from "@/lib/fmt";
import s from "./Proc.module.css";

type SignalName = "term" | "kill" | "hup";

/** 确认弹层的文案:说清后果,不写「确定吗」(spec §5.1)。 */
const SIGNALS: Record<SignalName, { label: string; consequence: string; destructive: boolean }> = {
  term: {
    label: "结束",
    consequence: "发送 SIGTERM,进程有机会保存状态后退出;未保存的工作可能丢失。",
    destructive: true,
  },
  kill: {
    label: "强制结束",
    consequence: "发送 SIGKILL,进程立即被杀,没有任何清理机会;未保存的工作一定丢失。",
    destructive: true,
  },
  hup: {
    label: "挂断",
    consequence: "发送 SIGHUP。守护进程通常以此重载配置;普通进程可能直接退出。",
    destructive: false,
  },
};

export function ProcDetail({ pid, onClose }: { pid: number; onClose: () => void }) {
  const [pending, setPending] = useState<SignalName | null>(null);
  const [actionErr, setActionErr] = useState<string | null>(null);
  const [actionNote, setActionNote] = useState<string | null>(null);
  const [nice, setNice] = useState<string>("");

  const detail = useQuery({
    queryKey: ["proc", pid],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/processes/{pid}", {
        params: { path: { pid } },
      });
      if (error) throw error;
      return data;
    },
    refetchInterval: 2_000,
    retry: 0,
  });

  const act = async (fn: () => Promise<{ error?: unknown }>, okNote: string) => {
    setActionErr(null);
    setActionNote(null);
    const { error } = await fn();
    if (error) {
      const e = error as { message?: string };
      setActionErr(e.message ?? "操作失败");
    } else {
      setActionNote(okNote);
    }
  };

  const sendSignal = (sig: SignalName) =>
    act(
      () =>
        api.POST("/api/v1/processes/{pid}/signal", {
          params: { path: { pid } },
          body: { signal: sig },
        }),
      `已发送 ${sig.toUpperCase()}`,
    );

  const applyNice = () => {
    const n = Number.parseInt(nice, 10);
    if (Number.isNaN(n) || n < -20 || n > 19) {
      setActionErr("nice 值必须在 -20..19 之间");
      return;
    }
    void act(
      () =>
        api.POST("/api/v1/processes/{pid}/renice", {
          params: { path: { pid } },
          body: { nice: n },
        }),
      `nice 已调到 ${n}`,
    );
  };

  const d = detail.data;

  const facts: KeyValueItem[] = [];
  if (d) {
    facts.push({
      k: "用户",
      v: `${d.user ?? d.uid}${d.euid !== undefined && d.euid !== null && d.euid !== d.uid ? `（有效 ${d.euid}）` : ""}`,
    });
    facts.push({
      k: "启动时间",
      v: new Date(d.start_ts * 1000).toLocaleString("zh-CN", { hour12: false }),
    });
    if (d.exe) facts.push({ k: "可执行文件", v: d.exe, mono: true });
    if (d.cwd) facts.push({ k: "工作目录", v: d.cwd, mono: true });
    if (d.tty) facts.push({ k: "TTY", v: d.tty, mono: true });
    if (d.unit) {
      // 进程 → 所属服务:点开服务页并直接选中该 unit
      const u = d.unit;
      facts.push({
        k: "服务单元",
        v: <Link to={`/services?unit=${encodeURIComponent(u)}`}>{u}</Link>,
        mono: true,
      });
    } else if (d.cgroup) facts.push({ k: "cgroup", v: d.cgroup, mono: true });
    facts.push({ k: "虚拟内存", v: fmtBytes(d.vms_bytes) });
    if (d.io_read_bytes !== undefined && d.io_read_bytes !== null)
      facts.push({
        k: "累计磁盘",
        v: `读 ${fmtBytes(d.io_read_bytes)} · 写 ${fmtBytes(d.io_write_bytes ?? 0)}`,
      });
    if (d.fds) facts.push({ k: "文件描述符", v: String(d.fds.length) });
  }

  return (
    <aside className={s.drawer} aria-label={`进程 ${pid} 详情`}>
      <div className={s.drawerHead}>
        <b className={s.drawerTitle}>{d?.name ?? "…"}</b>
        <span className={s.drawerPid}>PID {pid}</span>
        <Button size="sm" iconOnly aria-label="关闭详情" onClick={onClose}>
          <X size={14} strokeWidth={1.5} />
        </Button>
      </div>

      {detail.isError ? (
        <div className={s.drawerBody}>
          <ErrorState title="进程不存在" detail="可能刚刚退出了。" />
        </div>
      ) : (
        <div className={s.drawerBody}>
          {d && (
            <>
              {/* 动态读数,任务管理器式裸文本 */}
              <div className={s.drawerStats}>
                <div>
                  <div className={s.k}>CPU</div>
                  <div className={s.v}>{d.cpu_percent.toFixed(1)}%</div>
                </div>
                <div>
                  <div className={s.k}>内存</div>
                  <div className={s.v}>
                    {fmtBytes(d.rss_bytes)}
                    <small> {d.mem_percent.toFixed(1)}%</small>
                  </div>
                </div>
                {d.io_read_rate !== undefined && d.io_read_rate !== null && (
                  <div>
                    <div className={s.k}>磁盘 IO</div>
                    <div className={s.v}>
                      {fmtBytes((d.io_read_rate ?? 0) + (d.io_write_rate ?? 0))}/s
                    </div>
                  </div>
                )}
                <div>
                  <div className={s.k}>线程</div>
                  <div className={s.v}>{d.threads}</div>
                </div>
                <div>
                  <div className={s.k}>nice</div>
                  <div className={s.v}>{d.nice}</div>
                </div>
              </div>

              {(d.cmdline_args ?? []).length > 0 && (
                <div className={s.cmdBox} title="完整命令行">
                  {(d.cmdline_args ?? []).join(" ")}
                </div>
              )}

              <KeyValueGrid items={facts} columns={1} />

              {d.environ && Object.keys(d.environ).length > 0 && (
                <details className={s.env}>
                  {/* 环境变量常含密钥,默认折叠(API 文档同款要求) */}
                  <summary>环境变量（{Object.keys(d.environ).length}）</summary>
                  <div className={s.envList}>
                    {Object.entries(d.environ).map(([k, v]) => (
                      <div key={k}>
                        <b>{k}</b>={v}
                      </div>
                    ))}
                  </div>
                </details>
              )}

              <div className={s.actions}>
                <div className={s.actionRow}>
                  {(Object.keys(SIGNALS) as SignalName[]).map((sig) => (
                    <Button
                      key={sig}
                      size="sm"
                      variant={sig === "kill" ? "danger" : "secondary"}
                      onClick={() => setPending(sig)}
                    >
                      {SIGNALS[sig].label}
                    </Button>
                  ))}
                </div>
                <div className={s.actionRow}>
                  <label className={s.niceLabel}>
                    nice
                    <input
                      className={s.niceInput}
                      type="number"
                      min={-20}
                      max={19}
                      placeholder={String(d.nice)}
                      value={nice}
                      onChange={(e) => setNice(e.target.value)}
                    />
                  </label>
                  <Button size="sm" onClick={applyNice} disabled={nice === ""}>
                    调整优先级
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
        title={pending ? `${SIGNALS[pending].label} ${d?.name ?? `进程 ${pid}`}` : ""}
        confirmLabel={pending ? SIGNALS[pending].label : ""}
        destructive={pending ? SIGNALS[pending].destructive : false}
        onCancel={() => setPending(null)}
        onConfirm={() => {
          if (pending) void sendSignal(pending);
          setPending(null);
        }}
      >
        {pending ? SIGNALS[pending].consequence : null}
      </Dialog>
    </aside>
  );
}
