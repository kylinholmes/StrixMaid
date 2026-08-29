import { useState } from "react";
import { api } from "@/api/client";
import type { components } from "@/api/schema";
import { Button, Field } from "@/components";
import { useSession } from "@/session/useSession";
import s from "./Vault.module.css";

type Prompt = components["schemas"]["Prompt"];

interface ExtraRound {
  session: string;
  prompts: readonly Prompt[];
  /** prompt.id → 用户输入 */
  values: Record<number, string>;
}

function needsInput(p: Prompt): boolean {
  return p.style === "prompt" || p.style === "prompt_echo";
}

/**
 * PAM 多轮表单。常见情形（一轮、单个密码提示）在首屏一次收齐；
 * PAM 追问（2FA、改密码）时按服务端原文渲染追加轮次（spec：原样展示，不匹配内容做逻辑）。
 */
export function LoginForm({ hostLabel, osLabel }: { hostLabel: string; osLabel: string }) {
  const complete = useSession((st) => st.complete);

  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [extra, setExtra] = useState<ExtraRound | null>(null);

  async function respond(
    session: string,
    responses: { id: number; value: string }[],
    passwordLeft: string | null,
  ): Promise<void> {
    const { data, error: err } = await api.POST("/api/v1/auth/respond", {
      body: { session, responses },
    });
    if (err) {
      setError(err.message ?? "认证失败");
      setExtra(null);
      return;
    }
    if (data.status === "complete") {
      complete(data.token, {
        username: data.user.username,
        uid: data.user.uid,
        groups: data.user.groups ?? [],
      });
      setPassword("");
      return;
    }
    await drive(data.session, data.prompts, passwordLeft);
  }

  /** 处理一轮 prompts：能用密码自动作答就答，否则交给用户。 */
  async function drive(
    session: string,
    prompts: readonly Prompt[],
    passwordLeft: string | null,
  ): Promise<void> {
    const inputs = prompts.filter(needsInput);
    // 最常见的形状：恰好一个不回显提示（Password:）——用首屏收的密码直接作答
    const first = inputs[0];
    if (passwordLeft !== null && inputs.length === 1 && first && first.style === "prompt") {
      await respond(session, [{ id: first.id, value: passwordLeft }], null);
      return;
    }
    if (inputs.length === 0) {
      // 纯信息轮（提示语、过期警告），确认后继续
      await respond(session, [], passwordLeft);
      return;
    }
    setExtra({ session, prompts, values: {} });
  }

  async function submitInitial(e: React.FormEvent): Promise<void> {
    e.preventDefault();
    if (busy || !username) return;
    setBusy(true);
    setError(null);
    try {
      const { data, error: err } = await api.POST("/api/v1/auth/start", {
        body: { username },
      });
      if (err) {
        setError(err.message ?? "无法开始认证");
        return;
      }
      await drive(data.session, data.prompts ?? [], password);
    } finally {
      setBusy(false);
    }
  }

  async function submitExtra(e: React.FormEvent): Promise<void> {
    e.preventDefault();
    if (busy || !extra) return;
    setBusy(true);
    setError(null);
    try {
      const responses = extra.prompts
        .filter(needsInput)
        .map((p) => ({ id: p.id, value: extra.values[p.id] ?? "" }));
      await respond(extra.session, responses, null);
    } finally {
      setBusy(false);
    }
  }

  if (extra) {
    return (
      <form className={s.form} onSubmit={submitExtra}>
        <h1>继续验证</h1>
        <p className={s.formSub}>{username}</p>
        {extra.prompts.map((p) =>
          needsInput(p) ? (
            <Field
              key={p.id}
              label={p.text}
              type={p.style === "prompt" ? "password" : "text"}
              autoComplete={p.style === "prompt" ? "one-time-code" : undefined}
              value={extra.values[p.id] ?? ""}
              onChange={(e) =>
                setExtra({ ...extra, values: { ...extra.values, [p.id]: e.target.value } })
              }
            />
          ) : (
            <p key={p.id} className={p.style === "error" ? s.promptError : s.promptInfo}>
              {p.text}
            </p>
          ),
        )}
        {error && (
          <p className={s.errMsg} role="alert">
            {error}
          </p>
        )}
        <Button type="submit" variant="primary" disabled={busy}>
          {busy ? "验证中…" : "继续"}
        </Button>
      </form>
    );
  }

  return (
    <form className={s.form} onSubmit={submitInitial}>
      <h1>登录</h1>
      <p className={s.formSub}>{[hostLabel, osLabel].filter(Boolean).join(" · ") || "…"}</p>
      <Field
        label="用户名"
        autoComplete="username"
        value={username}
        onChange={(e) => setUsername(e.target.value)}
      />
      <Field
        label="密码"
        type="password"
        autoComplete="current-password"
        value={password}
        onChange={(e) => setPassword(e.target.value)}
      />
      {error && (
        <p className={s.errMsg} role="alert">
          {error}
        </p>
      )}
      <Button type="submit" variant="primary" disabled={busy || !username}>
        {busy ? "认证中…" : "登录"}
      </Button>
      <p className={s.hint}>以系统账户认证（PAM）。</p>
    </form>
  );
}
