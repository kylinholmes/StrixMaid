import type { components } from "@/api/schema";

export type Prompt = components["schemas"]["Prompt"];

export function needsInput(p: Prompt): boolean {
  return p.style === "prompt" || p.style === "prompt_echo";
}

/**
 * 一轮 prompts 的处理决定:
 * - `auto`:不打扰用户,直接以 `responses` 作答;`passwordAfter` 是作答后剩余的首屏密码
 *   (密码被消费掉就是 null,纯信息轮原样带过)。
 * - `ask`:交给用户逐项填写。
 */
export type RoundPlan =
  | { kind: "auto"; responses: { id: number; value: string }[]; passwordAfter: string | null }
  | { kind: "ask" };

/**
 * 规划一轮:最常见的形状——恰好一个不回显提示(Password:)——用首屏收的密码直接作答;
 * 纯信息轮(提示语、过期警告)空应答继续;其余(2FA、改密码、多项输入)交给用户。
 * spec:PAM 追问按服务端原文渲染,不匹配内容做逻辑。
 */
export function planRound(prompts: readonly Prompt[], passwordLeft: string | null): RoundPlan {
  const inputs = prompts.filter(needsInput);
  const first = inputs[0];
  if (passwordLeft !== null && inputs.length === 1 && first && first.style === "prompt") {
    return {
      kind: "auto",
      responses: [{ id: first.id, value: passwordLeft }],
      passwordAfter: null,
    };
  }
  if (inputs.length === 0) {
    return { kind: "auto", responses: [], passwordAfter: passwordLeft };
  }
  return { kind: "ask" };
}
