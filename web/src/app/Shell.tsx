import { useQuery } from "@tanstack/react-query";
import { ChevronDown, Lock, SunMoon, UserRound } from "lucide-react";
import { useEffect, useState } from "react";
import { Outlet, useLocation, useNavigate } from "react-router-dom";
import { api } from "@/api/client";
import { DistroMark, Menu, NavRail } from "@/components";
import { cx } from "@/lib/cx";
import { useLive } from "@/metrics/live";
import { useSession } from "@/session/useSession";
import { findDistro } from "@/theme/distro";
import { useTheme } from "@/theme/useTheme";
import { PAGE_GROUPS, pageAvailable } from "./pages";
import s from "./Shell.module.css";
import { capabilitiesQuery } from "./Vault";

/** 指标快照：外壳用它当「实时」心跳，概览页共用同一份缓存。 */
export function snapshotQuery(enabled: boolean) {
  return {
    queryKey: ["metrics", "current"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/metrics/current");
      if (error) throw error;
      return data;
    },
    refetchInterval: 2_000,
    enabled,
  } as const;
}

export function healthQuery(enabled: boolean) {
  return {
    queryKey: ["system", "health"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/system/health");
      if (error) throw error;
      return data;
    },
    refetchInterval: 30_000,
    enabled,
  } as const;
}

export function Shell() {
  const { status, user, lock } = useSession();
  const open = status === "open";
  const toggleMode = useTheme((t) => t.toggleMode);

  const caps = useQuery(capabilitiesQuery());
  const identity = caps.data?.identity;
  const distro = findDistro(identity?.os_id);

  const health = useQuery(healthQuery(open));
  const wsUp = useLive((st) => st.up);
  const lastTs = useLive((st) => st.lastTs);

  const navigate = useNavigate();
  const location = useLocation();
  const current = location.pathname.split("/")[1] || "overview";

  const [menuOpen, setMenuOpen] = useState(false);
  useEffect(() => {
    if (!menuOpen) return;
    const close = () => setMenuOpen(false);
    document.addEventListener("click", close);
    return () => document.removeEventListener("click", close);
  }, [menuOpen]);

  const sections = PAGE_GROUPS.map((g) => ({
    label: g.label,
    items: g.items
      .filter((p) => pageAvailable(p, caps.data?.system))
      .map((p) => ({ id: p.id, label: p.label, icon: p.icon })),
  })).filter((g) => g.items.length > 0);

  // 「实时」= WS 在线且 10 秒内收到过帧——不是 REST 心跳，WS 挂了就该灭
  const live = wsUp && lastTs > 0 && Date.now() / 1000 - lastTs < 10;
  const healthState = health.data?.status;
  const healthCls =
    healthState === "critical"
      ? s.sqBad
      : healthState === "warning"
        ? s.sqWarn
        : health.isSuccess
          ? undefined
          : s.sqPending;

  return (
    <div className={s.shell}>
      <aside className={s.rail}>
        <div className={s.menuAnchor}>
          <button
            type="button"
            className={s.machine}
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
              style={{ position: "absolute", left: 8, top: "100%", minWidth: 236, zIndex: 20 }}
              items={[
                { id: "current", label: `${identity?.hostname ?? "本机"}（当前）`, disabled: true },
                { id: "add", label: "添加机器…", disabled: true },
              ]}
              onPick={() => setMenuOpen(false)}
            />
          )}
        </div>
        <div className={s.hostat}>
          <i>
            <span className={cx(s.sq, !live && s.sqPending)} aria-hidden="true" />
            实时
          </i>
          <i>
            <span className={cx(s.sq, healthCls)} aria-hidden="true" />
            健康
          </i>
        </div>
        <div className={s.hr} />

        <NavRail
          bare
          sections={sections}
          current={current}
          onNavigate={(id) => navigate(`/${id}`)}
        />

        <div className={s.foot}>
          <button type="button" className={s.frow} onClick={() => void lock()}>
            <Lock size={15} aria-hidden="true" />
            锁定
          </button>
          <button type="button" className={s.frow} onClick={toggleMode}>
            <SunMoon size={15} aria-hidden="true" />
            主题
          </button>
          <div className={s.frow} style={{ cursor: "default" }}>
            <UserRound size={15} aria-hidden="true" />
            {user?.username ?? "—"}
          </div>
        </div>
      </aside>

      <main className={s.content}>
        <Outlet />
      </main>
    </div>
  );
}
