import { useQuery } from "@tanstack/react-query";
import { capabilitiesQuery } from "@/app/queries";
import { ErrorState } from "@/components";
import s from "./Login.module.css";
import { LoginForm } from "./LoginForm";

/**
 * 锁定态的内容区:居中的 PAM 表单。
 * helper 探测不到时不渲染表单——不给一个按下去必失败的框(spec §12)。
 */
export function LoginPanel() {
  const caps = useQuery(capabilitiesQuery());
  const identity = caps.data?.identity;
  const helperOk = caps.data?.system?.helper ?? true;

  return (
    <div className={s.panel}>
      {helperOk ? (
        <LoginForm hostLabel={identity?.hostname ?? ""} osLabel={identity?.os_name ?? ""} />
      ) : (
        <ErrorState
          title="无法登录：认证组件不可用"
          detail="strixmaid-helper 未部署或其 IPC socket 无法访问，PAM 认证走不通。部署 helper 后刷新此页。"
        />
      )}
    </div>
  );
}
