import { useQuery } from "@tanstack/react-query";
import { ChevronUp, Lock, SunMoon, UserRound } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import type { components } from "@/api/schema";
import { NavRail, Segmented } from "@/components";
import { cx } from "@/lib/cx";
import { useLive } from "@/metrics/live";
import { useSession } from "@/session/useSession";
import { type Distro, findDistro } from "@/theme/distro";
import { DESIGN_OPTIONS, pickTheme } from "@/theme/tokens";
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
        <ThemeRow />
        <div className={s.frow} style={{ cursor: "default" }}>
          <UserRound size={15} aria-hidden="true" />
          {user?.username ?? "—"}
        </div>
      </div>
    </div>
  );
}

/**
 * 底部的「主题」行:左半一点就换亮暗,右半的箭头开一个小弹层,
 * 明暗与设计语言两项都在里面。
 *
 * 为什么不是「点开弹层再选亮暗」:亮暗是一天要按好几次的动作,
 * 折进弹层就从一次点击变成两次。设计语言反过来,一年未必改一次,
 * 值不上一行常驻的位置。两者共用一行,是因为它们是同一件事的两个维度——
 * 界面长什么样。
 *
 * 为什么不新建设置页:项目今天没有设置页(`/settings` 还是占位页),
 * 为两个选项建一整页不合算。等设置页真的开工,这里的两项原样搬过去即可。
 *
 * 为什么设计语言用单选列表而不是 `Segmented`:明暗永远只有两档,分段控件正合适;
 * 设计语言的档数跟着 `THEMES` 长(第五版起是六套:StrixMaid / Fluent / macOS /
 * Adwaita / Breeze / Yaru),横排的分段控件在侧栏里撑不下,且会随清单变宽。
 * 竖排的原生单选自带方向键遍历与 `radiogroup` 语义,档数再多也只是变长,
 * 不必回头换控件——四套新语言加进来时这个组件一个字都没改。
 */
function ThemeRow() {
  const { mode, design, identity, setMode, toggleMode, setDesign } = useTheme();
  const [open, setOpen] = useState(false);
  const anchor = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const popId = useId();

  useEffect(() => {
    if (!open) return;
    // 只有点在弹层外面才关:里面的控件要能连着点几下(先换明暗再换设计语言)。
    // `useDismiss` 那套「点哪都关」是给点完即走的菜单用的,这里不适用。
    const onDown = (e: MouseEvent) => {
      if (!anchor.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      setOpen(false);
      // 关掉之后焦点要回到开它的那个按钮,否则键盘用户会被丢到页首
      trigger.current?.focus();
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div className={s.split} ref={anchor}>
      <button
        type="button"
        className={cx(s.frow, s.fmain)}
        aria-label={`主题:当前${mode === "dark" ? "暗色" : "亮色"},点击切换`}
        onClick={toggleMode}
      >
        <SunMoon size={15} aria-hidden="true" />
        主题
      </button>
      <button
        ref={trigger}
        type="button"
        className={cx(s.frow, s.fmore)}
        /* 展开/收起一段内容,用的是 disclosure 那一套(aria-expanded + aria-controls),
           不是 aria-haspopup="menu":里面是两组设置控件,不是一列菜单项 */
        aria-label="外观设置"
        aria-expanded={open}
        aria-controls={popId}
        onClick={() => setOpen((v) => !v)}
      >
        <ChevronUp size={14} aria-hidden="true" />
      </button>
      {open && (
        /* 弹层本身不挂 role:里面两组控件各自是 fieldset(分段控件那组的 legend
           是视觉隐藏的),名字由 legend 给。外面再套一层 group 只会多念一遍「外观」,
           开它的按钮已经叫这个名字 */
        <div className={s.pop} id={popId}>
          <div className={s.popRow}>
            <span className={s.popLabel}>明暗</span>
            <Segmented
              label="明暗"
              value={mode}
              onChange={setMode}
              options={[
                { value: "dark", label: "暗" },
                { value: "light", label: "亮" },
              ]}
            />
          </div>
          {/* 清单来自 DESIGN_OPTIONS(=「系统」+ 注册表里的每一套),不在这里写死:
              加一套设计语言时只加文件、在注册表里多一行,这里不必改 */}
          <fieldset className={s.radios}>
            <legend className={s.popLabel}>设计语言</legend>
            {DESIGN_OPTIONS.map((o) => (
              <label key={o.value} className={s.radio}>
                <input
                  type="radio"
                  /* 同一页上若出现第二个侧栏(登录门与外壳交替时),两组 radio
                     不能共用一个 name,否则会被浏览器当成同一组 */
                  name={`${popId}-design`}
                  value={o.value}
                  checked={design === o.value}
                  onChange={() => setDesign(o.value)}
                />
                <span>{o.label}</span>
                {o.value === "system" && (
                  <span className={s.popHint}>跟随这台机器,现为 {pickTheme(identity).name}</span>
                )}
              </label>
            ))}
          </fieldset>
        </div>
      )}
    </div>
  );
}
