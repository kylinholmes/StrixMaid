import { Outlet } from "react-router-dom";
import { LoginPanel } from "@/auth/LoginPanel";
import { useSession } from "@/session/useSession";
import { Rail } from "./Rail";
import s from "./Shell.module.css";

/**
 * 外壳 = 一体化侧栏 + 内容区。
 * 未认证(boot / locked)时内容区是登录表单,路由内容不挂载——锁上后页面数据就该不可见;
 * 认证后侧栏收窄、路由内容挂载。
 */
export function Shell() {
  const status = useSession((st) => st.status);
  const open = status === "open";

  return (
    <div className={s.shell}>
      <Rail locked={!open} />
      <main key={open ? "app" : "login"} className={s.content}>
        {open ? <Outlet /> : <LoginPanel />}
      </main>
    </div>
  );
}
