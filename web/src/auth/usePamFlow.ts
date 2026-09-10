import { useState } from "react";
import { api } from "@/api/client";
import { useSession } from "@/session/useSession";
import { needsInput, type Prompt, planRound } from "./pam";

interface ExtraRound {
  session: string;
  prompts: readonly Prompt[];
  /** prompt.id → 用户输入 */
  values: Record<number, string>;
}

/**
 * PAM 多轮认证的状态机。常见情形(一轮、单个密码提示)在首屏一次收齐;
 * PAM 追问(2FA、改密码)时进入 `extra` 轮,按服务端原文渲染。
 * 轮次决策在 `planRound`(纯函数,单测覆盖),这里只管驱动与状态。
 */
export function usePamFlow() {
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

  async function drive(
    session: string,
    prompts: readonly Prompt[],
    passwordLeft: string | null,
  ): Promise<void> {
    const plan = planRound(prompts, passwordLeft);
    if (plan.kind === "auto") {
      await respond(session, plan.responses, plan.passwordAfter);
      return;
    }
    setExtra({ session, prompts, values: {} });
  }

  async function submitCredentials(e: React.FormEvent): Promise<void> {
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

  function setExtraValue(id: number, value: string): void {
    setExtra((cur) => (cur ? { ...cur, values: { ...cur.values, [id]: value } } : cur));
  }

  return {
    username,
    setUsername,
    password,
    setPassword,
    busy,
    error,
    extra,
    setExtraValue,
    submitCredentials,
    submitExtra,
  };
}
