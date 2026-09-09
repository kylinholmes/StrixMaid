import { useQuery } from "@tanstack/react-query";
import { X } from "lucide-react";
import { api } from "@/api/client";
import { Button, ErrorState, KeyValueGrid, type KeyValueItem, Tag } from "@/components";
import s from "./Logs.module.css";

export function LogDetail({ cursor, onClose }: { cursor: string; onClose: () => void }) {
  const detail = useQuery({
    queryKey: ["logs", "entry", cursor],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/logs/entry/{cursor}", {
        params: { path: { cursor } },
      });
      if (error) throw error;
      return data;
    },
    retry: 0,
    staleTime: Number.POSITIVE_INFINITY, // 日志条目不可变
  });

  const d = detail.data;
  const facts: KeyValueItem[] = [];
  if (d) {
    facts.push({
      k: "时间",
      v: `${new Date(d.ts * 1000).toLocaleString("zh-CN", { hour12: false })}.${String(Math.floor(d.us / 1000)).padStart(3, "0")}`,
    });
    facts.push({ k: "级别", v: d.priority });
    if (d.unit) facts.push({ k: "unit", v: d.unit, mono: true });
    if (d.identifier) facts.push({ k: "标识", v: d.identifier, mono: true });
    if (d.pid !== undefined && d.pid !== null)
      facts.push({ k: "PID", v: String(d.pid), mono: true });
    if (d.uid !== undefined && d.uid !== null)
      facts.push({ k: "UID", v: String(d.uid), mono: true });
    if (d.transport) facts.push({ k: "传输", v: d.transport });
    if (d.hostname) facts.push({ k: "主机", v: d.hostname, mono: true });
    if (d.boot_id) facts.push({ k: "boot", v: d.boot_id, mono: true });
  }
  const fields = Object.entries(d?.fields ?? {});

  return (
    <aside className={s.drawer} aria-label="日志详情">
      <div className={s.drawerHead}>
        <span className={s.drawerTitle}>日志条目</span>
        {d?.transport && <Tag>{d.transport}</Tag>}
        <Button size="sm" iconOnly aria-label="关闭详情" onClick={onClose}>
          <X size={14} strokeWidth={1.5} />
        </Button>
      </div>
      {detail.isError ? (
        <div className={s.drawerBody}>
          <ErrorState title="条目不存在" detail="可能已被日志轮转淘汰,或对当前账户不可见。" />
        </div>
      ) : (
        <div className={s.drawerBody}>
          {d && (
            <>
              <div className={s.msgBox}>{d.message}</div>
              <KeyValueGrid items={facts} />
              {fields.length > 0 && (
                <details className={s.fields}>
                  <summary>全部字段（{fields.length}）</summary>
                  <div className={s.fieldList}>
                    {fields.map(([k, v]) => (
                      <div key={k}>
                        <b>{k}</b>={v}
                      </div>
                    ))}
                  </div>
                </details>
              )}
            </>
          )}
        </div>
      )}
    </aside>
  );
}
