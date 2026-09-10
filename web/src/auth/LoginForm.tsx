import { Button, Field } from "@/components";
import s from "./Login.module.css";
import { needsInput } from "./pam";
import { usePamFlow } from "./usePamFlow";

/** PAM 登录表单。多轮状态机在 usePamFlow,这里只管渲染。 */
export function LoginForm({ hostLabel, osLabel }: { hostLabel: string; osLabel: string }) {
  const flow = usePamFlow();

  if (flow.extra) {
    return (
      <form className={s.form} onSubmit={flow.submitExtra}>
        <h1>继续验证</h1>
        <p className={s.formSub}>{flow.username}</p>
        {flow.extra.prompts.map((p) =>
          needsInput(p) ? (
            <Field
              key={p.id}
              label={p.text}
              type={p.style === "prompt" ? "password" : "text"}
              autoComplete={p.style === "prompt" ? "one-time-code" : undefined}
              value={flow.extra?.values[p.id] ?? ""}
              onChange={(e) => flow.setExtraValue(p.id, e.target.value)}
            />
          ) : (
            <p key={p.id} className={p.style === "error" ? s.promptError : s.promptInfo}>
              {p.text}
            </p>
          ),
        )}
        {flow.error && (
          <p className={s.errMsg} role="alert">
            {flow.error}
          </p>
        )}
        <Button type="submit" variant="primary" disabled={flow.busy}>
          {flow.busy ? "验证中…" : "继续"}
        </Button>
      </form>
    );
  }

  return (
    <form className={s.form} onSubmit={flow.submitCredentials}>
      <h1>登录</h1>
      <p className={s.formSub}>{[hostLabel, osLabel].filter(Boolean).join(" · ") || "…"}</p>
      <Field
        label="用户名"
        autoComplete="username"
        value={flow.username}
        onChange={(e) => flow.setUsername(e.target.value)}
      />
      <Field
        label="密码"
        type="password"
        autoComplete="current-password"
        value={flow.password}
        onChange={(e) => flow.setPassword(e.target.value)}
      />
      {flow.error && (
        <p className={s.errMsg} role="alert">
          {flow.error}
        </p>
      )}
      <Button type="submit" variant="primary" disabled={flow.busy || !flow.username}>
        {flow.busy ? "认证中…" : "登录"}
      </Button>
      <p className={s.hint}>以系统账户认证（PAM）。</p>
    </form>
  );
}
