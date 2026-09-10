import { describe, expect, it } from "vitest";
import { needsInput, type Prompt, planRound } from "./pam";

const pw = (id: number, text = "Password:"): Prompt => ({ id, text, style: "prompt" });
const echo = (id: number, text = "Login:"): Prompt => ({ id, text, style: "prompt_echo" });
const info = (id: number, text = "notice"): Prompt => ({ id, text, style: "info" });
const err = (id: number, text = "bad"): Prompt => ({ id, text, style: "error" });

describe("needsInput", () => {
  it("只有 prompt / prompt_echo 需要输入", () => {
    expect(needsInput(pw(1))).toBe(true);
    expect(needsInput(echo(1))).toBe(true);
    expect(needsInput(info(1))).toBe(false);
    expect(needsInput(err(1))).toBe(false);
  });
});

describe("planRound", () => {
  it("单个不回显提示 + 手上有密码 → 自动作答并消费密码", () => {
    expect(planRound([pw(3)], "s3cret")).toEqual({
      kind: "auto",
      responses: [{ id: 3, value: "s3cret" }],
      passwordAfter: null,
    });
  });

  it("信息提示夹着单个密码提示,仍走自动作答", () => {
    const plan = planRound([info(1), pw(2)], "s3cret");
    expect(plan).toMatchObject({ kind: "auto", responses: [{ id: 2, value: "s3cret" }] });
  });

  it("纯信息轮 → 空应答继续,密码原样留着", () => {
    expect(planRound([info(1), err(2)], "s3cret")).toEqual({
      kind: "auto",
      responses: [],
      passwordAfter: "s3cret",
    });
  });

  it("密码已被消费(null)后再来提示 → 交给用户", () => {
    expect(planRound([pw(1)], null)).toEqual({ kind: "ask" });
  });

  it("回显提示(2FA code 之类)不能拿密码去填 → 交给用户", () => {
    expect(planRound([echo(1, "Verification code:")], "s3cret")).toEqual({ kind: "ask" });
  });

  it("多个输入项 → 交给用户", () => {
    expect(planRound([pw(1, "New password:"), pw(2, "Retype:")], "s3cret")).toEqual({
      kind: "ask",
    });
  });
});
