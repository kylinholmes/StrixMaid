import { useQuery } from "@tanstack/react-query";
import { capabilitiesQuery } from "@/app/queries";
import { ErrorState } from "@/components";
import s from "./Login.module.css";
import { LoginForm } from "./LoginForm";

/**
 * 锁定态的内容区:居中的登录表单。
 * helper 探测不到时不渲染表单——不给一个按下去必失败的框(spec §12)。
 */
export function LoginPanel() {
  const caps = useQuery(capabilitiesQuery());
  const identity = caps.data?.identity;
  const helperOk = caps.data?.system?.helper ?? true;
  // 认证机制和 IPC 通道都是按平台分的,说错了会把人引去找这台机器上根本不存在的东西
  const detail =
    identity?.os_id === "windows"
      ? "strixmaid-helper 未部署,或其命名管道无法访问。部署 helper 后刷新此页。"
      : "strixmaid-helper 未部署,或其 IPC socket 无法访问,PAM 认证走不通。部署 helper 后刷新此页。";

  return (
    <div className={s.panel}>
      {helperOk ? (
        <LoginForm hostLabel={identity?.hostname ?? ""} osLabel={identity?.os_name ?? ""} />
      ) : (
        <ErrorState title="无法登录：认证组件不可用" detail={detail} />
      )}
    </div>
  );
}
