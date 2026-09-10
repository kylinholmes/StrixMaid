import { useState } from "react";
import { api } from "@/api/client";
import type { components } from "@/api/schema";
import { Dialog, Segmented } from "@/components";
import { fmtBytes } from "@/lib/fmt";
import s from "./Logs.module.css";

type LogUsage = components["schemas"]["LogUsage"];
type VacuumMode = components["schemas"]["VacuumMode"];

const MODE_LABEL: Record<VacuumMode, string> = {
  keep_duration: "按保留期",
  max_size: "按目标大小",
  erase_all: "全部抹除",
};

export function VacuumDialog({
  open,
  usage,
  onClose,
  onDone,
}: {
  open: boolean;
  usage: LogUsage | undefined;
  onClose: () => void;
  /** 清理成功:结果文案交给页面展示(对话框已关) */
  onDone: (msg: string) => void;
}) {
  const modes = usage?.modes ?? [];
  const [mode, setMode] = useState<VacuumMode | null>(null);
  const [keepDays, setKeepDays] = useState("7");
  const [maxMb, setMaxMb] = useState("500");
  const [eraseAck, setEraseAck] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const active: VacuumMode | null = mode ?? modes[0] ?? null;
  if (!open || active === null) return null;

  const confirm = () => {
    if (busy) return;
    setErr(null);
    let body: { keep_secs?: number; max_bytes?: number; erase_all?: boolean };
    if (active === "keep_duration") {
      const days = Number.parseFloat(keepDays);
      if (!(days > 0)) {
        setErr("保留天数必须大于 0");
        return;
      }
      body = { keep_secs: Math.round(days * 86_400) };
    } else if (active === "max_size") {
      const mb = Number.parseFloat(maxMb);
      if (!(mb > 0)) {
        setErr("目标大小必须大于 0");
        return;
      }
      body = { max_bytes: Math.round(mb * 1024 * 1024) };
    } else {
      if (!eraseAck) {
        setErr("请先勾选确认——这会抹掉全部系统日志,无法恢复");
        return;
      }
      body = { erase_all: true };
    }
    setBusy(true);
    void (async () => {
      const { data, error } = await api.POST("/api/v1/logs/vacuum", { body });
      setBusy(false);
      if (error) {
        const e = error as { message?: string; detail?: string; can_retry_elevated?: boolean };
        // detail 是底层工具的原话,不吞——「失败」两个字没法排障
        const parts = [e.message ?? "清理失败"];
        if (e.detail) parts.push(e.detail);
        if (e.can_retry_elevated) parts.push("启用管理访问后重试");
        setErr(parts.join(" · "));
        return;
      }
      const freed =
        data?.before_bytes !== undefined &&
        data.before_bytes !== null &&
        data.after_bytes !== undefined &&
        data.after_bytes !== null
          ? ` · ${fmtBytes(data.before_bytes)} → ${fmtBytes(data.after_bytes)}`
          : "";
      onDone(`${data?.detail ?? "清理完成"}${freed}`);
      setEraseAck(false);
      onClose();
    })();
  };

  return (
    <Dialog
      open={open}
      title="清理日志"
      confirmLabel={busy ? "清理中…" : MODE_LABEL[active]}
      destructive
      onCancel={onClose}
      onConfirm={confirm}
    >
      <p className={s.vacNote}>
        当前占用:
        {usage?.bytes !== undefined && usage?.bytes !== null ? fmtBytes(usage.bytes) : "测不到"}
      </p>
      {modes.length > 1 && (
        <div className={s.vacRow}>
          <Segmented
            label="清理方式"
            value={active}
            onChange={(v: VacuumMode) => setMode(v)}
            options={modes.map((m) => ({ value: m, label: MODE_LABEL[m] }))}
          />
        </div>
      )}
      {active === "keep_duration" && (
        <div className={s.vacRow}>
          只保留最近
          <input
            className={s.vacInput}
            type="number"
            min={1}
            value={keepDays}
            onChange={(e) => setKeepDays(e.target.value)}
          />
          天,更早的归档删除。
        </div>
      )}
      {active === "max_size" && (
        <div className={s.vacRow}>
          收缩到
          <input
            className={s.vacInput}
            type="number"
            min={1}
            value={maxMb}
            onChange={(e) => setMaxMb(e.target.value)}
          />
          MB 以内。
        </div>
      )}
      {(active === "keep_duration" || active === "max_size") && (
        <p className={s.vacNote}>
          journald 只清<b>已归档</b>的日志文件,当前活跃文件不动——清理后的占用不会精确等于期望值。
        </p>
      )}
      {active === "erase_all" && (
        <>
          <p className={s.eraseWarn}>
            抹掉这台机器上统一日志的<b>全部</b>归档——所有历史日志立即消失,无法恢复。 macOS
            没有「只清一部分」的接口,这是唯一一档。
          </p>
          <label className={s.vacRow}>
            <input
              type="checkbox"
              checked={eraseAck}
              onChange={(e) => setEraseAck(e.target.checked)}
            />
            我明白全部日志将被抹除
          </label>
        </>
      )}
      {err && <p className={s.vacErr}>{err}</p>}
    </Dialog>
  );
}
