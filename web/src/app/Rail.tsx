import { useQuery } from "@tanstack/react-query";
import { Lock, SunMoon, UserRound } from "lucide-react";
import { useLocation, useNavigate } from "react-router-dom";
import type { components } from "@/api/schema";
import { NavRail } from "@/components";
import { cx } from "@/lib/cx";
import { useLive } from "@/metrics/live";
import { useSession } from "@/session/useSession";
import { type Distro, findDistro } from "@/theme/distro";
import { useTheme } from "@/theme/useTheme";
import { MachineMenu } from "./MachineMenu";
import { PAGE_GROUPS, pageAvailable, type SystemCaps } from "./pages";
import { capabilitiesQuery, healthQuery } from "./queries";
import s from "./Rail.module.css";

type Identity = components["schemas"]["Capabilities"]["identity"];

/**
 * 一体化侧栏:登录门是它的宽形态(322px、恒暗、印着进去后的目录),
 * 认证通过后原地收窄成导航栏(184px、跟随主题)。同一个组件,两种内容。
 */
export function Rail({ locked }: { locked: boolean }) {
  const caps = useQuery(capabilitiesQuery());
  const identity = caps.data?.identity;
  const sys = caps.data?.system;
  const distro = findDistro(identity?.os_id);

  return (
    <aside
      className={cx(s.rail, locked && s.locked)}
      /* 恒暗的锁定形态永远配暗档 accent,与主题无关 */
      style={{ "--accent-dark": distro.accent.dark } as React.CSSProperties}
    >
      {locked ? (
        <LockedBody identity={identity} distro={distro} sys={sys} />
      ) : (
        <OpenBody identity={identity} distro={distro} sys={sys} />
      )}
    </aside>
  );
}

interface BodyProps {
  identity: Identity | undefined;
  distro: Distro;
  sys: SystemCaps | undefined;
}

/** 锁定形态:机器身份 + 页面目录(门上印着进去后能看到什么)+ 认证状态。 */
function LockedBody({ identity, distro, sys }: BodyProps) {
  const pages = PAGE_GROUPS.flatMap((g) => g.items);
  const availableCount = pages.filter((p) => pageAvailable(p, sys)).length;
  const helperOk = sys?.helper ?? true;

  return (
    <div className={s.body}>
      <div className={s.head}>
        <MachineMenu hostname={identity?.hostname} distro={distro} wide />
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

      <div className={s.lockFoot}>
        <span>StrixMaid</span>
        <span>
          {availableCount} / {pages.length} 页可用
        </span>
      </div>
    </div>
  );
}

/** 导航形态:机器行 + 实时/健康 + 导航 + 底部动作。 */
function OpenBody({ identity, distro, sys }: BodyProps) {
  const { user, lock } = useSession();
  const toggleMode = useTheme((t) => t.toggleMode);

  const health = useQuery(healthQuery(true));
  const wsUp = useLive((st) => st.up);
  const lastTs = useLive((st) => st.lastTs);

  const navigate = useNavigate();
  const location = useLocation();
  const current = location.pathname.split("/")[1] || "overview";

  const sections = PAGE_GROUPS.map((g) => ({
    label: g.label,
    items: g.items
      .filter((p) => pageAvailable(p, sys))
      .map((p) => ({ id: p.id, label: p.label, icon: p.icon })),
  })).filter((g) => g.items.length > 0);

  // 「实时」= WS 在线且 10 秒内收到过帧——不是 REST 心跳,WS 挂了就该灭
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
    <div className={s.body}>
      <MachineMenu hostname={identity?.hostname} distro={distro} />
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

      <NavRail bare sections={sections} current={current} onNavigate={(id) => navigate(`/${id}`)} />

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
    </div>
  );
}
