import { useQuery } from "@tanstack/react-query";
import { ChevronDown } from "lucide-react";
import { useEffect, useState } from "react";
import { api } from "@/api/client";
import type { components } from "@/api/schema";
import { DistroMark, ErrorState, Menu } from "@/components";
import { cx } from "@/lib/cx";
import { useSession } from "@/session/useSession";
import { findDistro } from "@/theme/distro";
import { LoginForm } from "./LoginForm";
import { PAGE_GROUPS, pageAvailable } from "./pages";
import s from "./Vault.module.css";

type Capabilities = components["schemas"]["Capabilities"];

export function capabilitiesQuery() {
  return {
    queryKey: ["capabilities"],
    queryFn: async (): Promise<Capabilities> => {
      const { data, error } = await api.GET("/api/v1/capabilities");
      if (error) throw error;
      return data;
    },
    staleTime: 60_000,
  } as const;
}

/**
 * 保险箱：登录页是盖在应用上的两扇门。
 *
 * 左门（丙案）：机器行 + 页面清单——门上印着进去后的目录，缺能力的页面划线标「未检出」。
 * 右门：PAM 多轮表单。helper 探测不到时不渲染表单——不给一个按下去必失败的框。
 */
export function Vault() {
  const status = useSession((st) => st.status);
  const open = status === "open";

  const caps = useQuery(capabilitiesQuery());
  const identity = caps.data?.identity;
  const sys = caps.data?.system;
  const distro = findDistro(identity?.os_id);
  const helperOk = sys?.helper ?? true;

  const [menuOpen, setMenuOpen] = useState(false);
  useEffect(() => {
    if (!menuOpen) return;
    const close = () => setMenuOpen(false);
    document.addEventListener("click", close);
    return () => document.removeEventListener("click", close);
  }, [menuOpen]);

  const pages = PAGE_GROUPS.flatMap((g) => g.items);
  const availableCount = pages.filter((p) => pageAvailable(p, sys)).length;

  return (
    <div className={cx(s.vault, open && s.open)} aria-hidden={open}>
      <aside
        className={cx(s.door, s.left)}
        style={{ "--accent-door": distro.accent.dark } as React.CSSProperties}
      >
        <div className={s.head}>
          <div className={s.menuAnchor}>
            <button
              type="button"
              className={s.idBtn}
              aria-haspopup="menu"
              aria-expanded={menuOpen}
              onClick={(e) => {
                e.stopPropagation();
                setMenuOpen((v) => !v);
              }}
            >
              <DistroMark distro={distro} />
              <span className={s.host}>{identity?.hostname || "…"}</span>
              <ChevronDown size={12} className={s.chev} aria-hidden="true" />
            </button>
            {menuOpen && (
              <Menu
                label="机器"
                style={{ position: "absolute", left: 0, top: "100%", minWidth: 236 }}
                items={[
                  {
                    id: "current",
                    label: `${identity?.hostname ?? "本机"}（当前）`,
                    disabled: true,
                  },
                  { id: "add", label: "添加机器…", disabled: true },
                ]}
                onPick={() => setMenuOpen(false)}
              />
            )}
          </div>
          <div className={s.meta}>
            {identity ? `${identity.os_name || "未知系统"} · 内核 ${identity.kernel}` : "正在探测…"}
          </div>
        </div>

        <div className={s.hr} />

        <div className={s.dir}>
          {PAGE_GROUPS.map((g) => (
            <div key={g.label}>
              <h2 className={s.sec}>{g.label}</h2>
              {g.items.map((p) => {
                const on = pageAvailable(p, sys);
                return (
                  <div key={p.id} className={cx(s.pg, !on && s.pgNa)}>
                    <span className={cx(s.sq, !on && s.sqNa)} aria-hidden="true" />
                    <span className={s.nm}>{p.label}</span>
                    <span className={s.via}>{on ? p.via : "未检出"}</span>
                  </div>
                );
              })}
            </div>
          ))}
        </div>

        <div className={s.auth}>
          <span className={cx(s.sq, !helperOk && s.sqBad)} aria-hidden="true" />
          身份认证
          <span className={s.via}>{helperOk ? "PAM" : "helper 不可用"}</span>
        </div>

        <div className={s.foot}>
          <span>StrixMaid</span>
          <span>
            {availableCount} / {pages.length} 页可用
          </span>
        </div>
      </aside>

      <main className={cx(s.door, s.right)}>
        {helperOk ? (
          <LoginForm hostLabel={identity?.hostname ?? ""} osLabel={identity?.os_name ?? ""} />
        ) : (
          <ErrorState
            title="无法登录：认证组件不可用"
            detail="strixmaid-helper 未部署或其 IPC socket 无法访问，PAM 认证走不通。部署 helper 后刷新此页。"
          />
        )}
      </main>
    </div>
  );
}
