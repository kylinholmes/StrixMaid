import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

/**
 * 防回潮：扫源码，挡住写死的值重新长回 CSS 里。
 *
 * 把值搬进 token 是一次性的，**保持**它在 token 里不是。下一次有人赶工加个
 * 面板，顺手写 `border-radius: 4px` `font-size: 13px`，Fluent 那边就多一处
 * 不跟着走的地方——而且这种回潮不报错、不掉测试，只有换到另一套语言截图时
 * 才看得出来。所以这一条检查是这次重构能不能留住的关键。
 *
 * 扫三类，都是**换语言时一定要变、写死就一定错**的：
 *
 * - 圆角：StrixMaid 全 0、Fluent 2/4/8/胶囊，写死等于钉死其中一套；
 * - 过渡与动画的时长：两套语言的时长阶不同；
 * - 像素字号：两套语言的字阶不同。
 *
 * 颜色不在扫描范围内——它早在第二版就全进 token 了，而且 `#` 开头的字面量
 * 在图标、品牌方块这些地方有正当用途，扫出来全是噪音。
 *
 * ## 白名单
 *
 * 白名单不是「暂时绕过」的口子，每一条都要写清**为什么这一处不该 token 化**。
 * 下面那条「白名单里的每一条都必须还能扫得到」的断言是配套的：某处改好了、
 * 或者那条规则不再触发，对应的白名单条目就必须删掉，不许留着积灰。
 */

const SRC = fileURLToPath(new URL("..", import.meta.url));

/** 被扫的范围：23 个 CSS 模块 + 全局的两个。样式全在这些文件里。 */
function stylesheets(dir: string): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) out.push(...stylesheets(path));
    else if (entry.endsWith(".css")) out.push(path);
  }
  return out;
}

const FILES = stylesheets(SRC).map((p) => ({
  /** 相对 src 的路径，正斜杠。白名单里写的就是这个形状 */
  name: relative(SRC, p).split(sep).join("/"),
  // 注释先抹掉再扫。注释里既会出现被解释的写死值（「原来是 13px」），
  // 也会出现本检查自己的说明文字，不抹的话检查会被自己的文档绊倒。
  text: readFileSync(p, "utf8").replace(/\/\*[\s\S]*?\*\//g, ""),
}));

interface Finding {
  readonly file: string;
  readonly decl: string;
  readonly rule: string;
}

/** 一条声明里的 `var(...)` 全部抹掉，剩下的才是真的写死了的部分。 */
function withoutVars(value: string): string {
  let out = value;
  let prev = "";
  while (out !== prev) {
    prev = out;
    out = out.replace(/var\([^()]*\)/g, "");
  }
  return out;
}

function scan(): Finding[] {
  const found: Finding[] = [];
  for (const { name, text } of FILES) {
    for (const m of text.matchAll(/([a-z-]+)\s*:\s*([^;{}]+);/g)) {
      const prop = m[1]!;
      const raw = m[2]!.trim();
      const bare = withoutVars(raw);
      const decl = `${prop}: ${raw};`;

      // 圆角：0 是允许的（base.css 那条重置本来就该是 0），其余必须走 token
      if (prop === "border-radius" && bare.trim() !== "0" && /\d/.test(bare)) {
        found.push({ file: name, decl, rule: "border-radius" });
      }
      // 时长：transition / animation 的简写与专门那一条都算
      if (
        (prop === "transition" ||
          prop === "transition-duration" ||
          prop === "animation" ||
          prop === "animation-duration") &&
        /(^|[\s(,])\d*\.?\d+m?s\b/.test(bare)
      ) {
        found.push({ file: name, decl, rule: "duration" });
      }
      // 像素字号：`font-size` 与 `font` 简写
      if ((prop === "font-size" || prop === "font") && /\d*\.?\d+px/.test(bare)) {
        found.push({ file: name, decl, rule: "font-size" });
      }
    }
  }
  return found;
}

/**
 * 每一条都写明为什么这一处不该走 token。
 *
 * 匹配按「文件 + 声明原文」精确比对，不做模糊匹配：改了值就得回来重新看一眼，
 * 这正是要的效果。
 */
const ALLOWED: readonly { file: string; decl: string; reason: string }[] = [
  {
    file: "styles/base.css",
    decl: "transition-duration: 0.001ms !important;",
    reason:
      "prefers-reduced-motion 的降级。这个值必须是一个具体的、接近 0 的时长：" +
      "走 token 就等于给了设计语言「把这条无障碍降级关掉」的能力，那是不该存在的能力。" +
      "不写 0 是因为 0 会让某些浏览器不派发 transitionend，依赖它的代码会卡住。",
  },
  {
    file: "styles/base.css",
    decl: "animation-duration: 0.001ms !important;",
    reason: "同上，animation 侧的那一半。",
  },
  {
    file: "components/States.module.css",
    decl: "animation: slide 1.1s var(--ease-in-out) infinite;",
    reason:
      "不定量进度条的循环周期，不是「一次过渡有多快」。时长轴上的六档回答的是" +
      "「状态变化该用多久」，循环动画的周期是节奏，换一套设计语言不该跟着变——" +
      "1.1s 是照着进度条自身的长度与速度感定的。缓动仍然走 token。",
  },
  {
    file: "logs/Logs.module.css",
    decl: "font: 12px inherit;",
    reason:
      "这是一条**解析期就无效**的声明（`inherit` 不能作 font 简写的分量），" +
      "浏览器整条丢弃，按钮的字号实际继承自 body。换成 `var(--fs-400)` 会让它从" +
      "「解析期无效」变成「计算期无效」，按规范回落到 font 的初始值（medium/16px），" +
      "那是真的视觉变化。这一轮只搬值不改行为，留着；要清理请单独一轮，" +
      "把这三处连同后面那行 `font-family: inherit` 一起改成 `font-size: var(--fs-400)`。",
  },
  {
    file: "proc/Proc.module.css",
    decl: "font: 12px inherit;",
    reason: "同 logs/Logs.module.css 的那一条（.userBtn，与 .pickBtn 同规格）。",
  },
  {
    file: "svc/Svc.module.css",
    decl: "font: 12px inherit;",
    reason: "同 logs/Logs.module.css 的那一条（.pickBtn，三个页面同规格）。",
  },
];

function isAllowed(f: Finding): boolean {
  return ALLOWED.some((a) => a.file === f.file && a.decl === f.decl);
}

describe("防回潮：CSS 里不得再出现写死的值", () => {
  it("扫到的文件数量对得上——扫不到文件的检查等于没有检查", () => {
    expect(FILES.length).toBeGreaterThanOrEqual(25);
    expect(FILES.some((f) => f.name === "styles/base.css")).toBe(true);
    expect(FILES.filter((f) => f.name.endsWith(".module.css")).length).toBeGreaterThanOrEqual(23);
  });

  it("没有写死的圆角、过渡时长、像素字号（白名单之外）", () => {
    const bad = scan().filter((f) => !isAllowed(f));
    expect(
      bad.map((f) => `${f.file}  [${f.rule}]  ${f.decl}`),
      "上面这些请改成 var(--radius-* / --t-* / --fs-*)；确实不该 token 化的，" +
        "加进本文件的 ALLOWED 并写明理由",
    ).toEqual([]);
  });

  // 白名单会积灰：某一处改好了、或者规则不再触发，条目还留着，下一个人就以为
  // 那里仍然有个不能碰的坑。这一条逼着它跟着现实走。
  it("白名单里的每一条都还扫得到，没有过期的条目", () => {
    const all = scan();
    for (const a of ALLOWED) {
      expect(
        all.some((f) => f.file === a.file && f.decl === a.decl),
        `白名单条目已经过期，请删掉：${a.file} ${a.decl}`,
      ).toBe(true);
    }
  });
});

/**
 * 作用域覆盖的数量，是「一套语言一个文件」这个方案成不成立的判据。
 *
 * 每多一处 `[data-design="…"]`，就多一处「换语言时要去 CSS 里改」的地方，
 * 方案的价值就少一分。真的表达不了的结构差异才配用它；三五处以上就说明
 * 轴划漏了，该回去补轴而不是继续加分支。
 *
 * 今天全站**只有一处**：Fluent 的双描边焦点环（base.css）。
 * 环的颜色、宽度、偏移全部走 token，覆盖里只有「多画一层」这一件事——
 * `outline` 画不出第二层不同色的环。
 */
describe("[data-design] 作用域覆盖", () => {
  const hits = FILES.flatMap(({ name, text }) =>
    [...text.matchAll(/\[data-design[^\]]*\]/g)].map(() => name),
  );

  it("只有一处，就在 base.css 的焦点环上", () => {
    expect(hits).toEqual(["styles/base.css"]);
  });
});
