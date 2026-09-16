import { afterEach, describe, expect, it, vi } from "vitest";
import { DESIGN_OPTIONS, FLUENT_THEME, GENERIC_THEME, THEMES } from "./tokens";

/**
 * 主题 store 的行为，重点在**设计语言偏好**：默认档、指名某一套、跨刷新保持、
 * 非法值回落，以及「换设计语言不动 accent」。
 *
 * 测试跑在 node 环境（本项目没有装 jsdom），所以 `localStorage` 与 `document`
 * 都由这里现造。造的是最小可用的那一点：store 只用到 `getItem` / `setItem`，
 * `applyTheme` 只用到 `documentElement.style.setProperty` 与 `dataset`。
 * 用真浏览器 API 才能测的东西（动画、焦点）不在这一层测。
 *
 * 「刷新」= `vi.resetModules()` 之后重新 import：store 是模块级单例，
 * 初始值在 import 那一刻从 localStorage 读出来，重新 import 走的正是那条路径。
 * 代价是每次 import 连 `./tokens` 也是新实例，所以**不要用 `toBe` 比主题对象**，
 * 比 `theme.id`（字符串在两个实例之间仍然相等）。
 */

/** 最小 localStorage。与浏览器一致：只存字符串，取不到给 null。 */
function fakeStorage(seed: Record<string, string> = {}) {
  const map = new Map(Object.entries(seed));
  return {
    map,
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, String(v)),
    removeItem: (k: string) => void map.delete(k),
  };
}

/** 最小 document。落下去的 CSS 变量收进 map，断言直接看这里。 */
function fakeDocument(vars: Map<string, string>) {
  return {
    documentElement: {
      style: { setProperty: (k: string, v: string) => void vars.set(k, v) },
      dataset: {} as Record<string, string>,
    },
  };
}

/** 开一次「浏览器」：给定这份存储，重新加载 store 模块。 */
async function boot(storage: ReturnType<typeof fakeStorage> | undefined = fakeStorage()) {
  const vars = new Map<string, string>();
  vi.resetModules();
  vi.stubGlobal("localStorage", storage);
  vi.stubGlobal("document", fakeDocument(vars));
  const { useTheme } = await import("./useTheme");
  return { store: useTheme, vars };
}

const DESIGN_KEY = "strixmaid.theme.design";

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("设计语言偏好", () => {
  it("没存过偏好时默认是「系统」，Windows 于是拿到 Fluent", async () => {
    const { store } = await boot();
    expect(store.getState().design).toBe("system");

    store.getState().setPlatform({ osId: "windows" });
    expect(store.getState().theme.id).toBe(FLUENT_THEME.id);
  });

  it("「系统」档下 Linux 与 macOS 仍是 StrixMaid 那套——与加这个设置之前零差异", async () => {
    const { store } = await boot();
    for (const osId of ["ubuntu", "debian", "macos", null]) {
      store.getState().setPlatform({ osId });
      expect(store.getState().theme.id, String(osId)).toBe(GENERIC_THEME.id);
    }
  });

  // 遍历清单而不是列举两档：加一套设计语言时这条自动覆盖它。
  it("清单里的每一档都选得动，选了之后连 Windows 也照办", async () => {
    for (const o of DESIGN_OPTIONS) {
      if (o.value === "system") continue;
      const { store } = await boot();
      store.getState().setPlatform({ osId: "windows" });
      store.getState().setDesign(o.value);
      expect(store.getState().theme.id, o.value).toBe(o.value);
    }
  });

  it("切到 StrixMaid 后 Windows 拿到 StrixMaid 那套，CSS 变量也真落下去了", async () => {
    const { store, vars } = await boot();
    store.getState().setPlatform({ osId: "windows" });
    expect(store.getState().theme.id).toBe(FLUENT_THEME.id);

    store.getState().setDesign(GENERIC_THEME.id);
    const { theme, mode } = store.getState();
    expect(theme.id).toBe(GENERIC_THEME.id);
    // 光看 store 不够：换了主题但没重写 :root，界面上什么都不会变
    expect(vars.get("--surface")).toBe(GENERIC_THEME.tokens[mode].surface);
    expect(vars.get("--ink")).toBe(GENERIC_THEME.tokens[mode].ink);
  });

  it("先选偏好后认出机器，结果与反过来一致", async () => {
    const { store } = await boot();
    store.getState().setDesign(GENERIC_THEME.id);
    store.getState().setPlatform({ osId: "windows" });
    expect(store.getState().theme.id).toBe(GENERIC_THEME.id);

    // 切回「系统」时要能重新认出这台机器——store 必须记着 identity，
    // 只记算完的 theme 的话这一步就没有东西可以重算了
    store.getState().setDesign("system");
    expect(store.getState().theme.id).toBe(FLUENT_THEME.id);
  });

  it("accent 不受设计语言影响：换成任何一套，Windows 仍然是 Fluent 蓝", async () => {
    const { store, vars } = await boot();
    store.getState().setPlatform({ osId: "windows" });
    const accent = vars.get("--accent");

    for (const t of THEMES) {
      store.getState().setDesign(t.id);
      const { distro, mode } = store.getState();
      expect(distro.id, t.id).toBe("windows");
      expect(vars.get("--accent"), t.id).toBe(accent);
      expect(vars.get("--accent"), t.id).toBe(distro.accent[mode]);
    }
  });

  it("偏好写进 localStorage，刷新后仍然保持", async () => {
    const storage = fakeStorage();
    const first = await boot(storage);
    first.store.getState().setDesign(GENERIC_THEME.id);
    expect(storage.map.get(DESIGN_KEY)).toBe(GENERIC_THEME.id);

    // 同一份存储再开一次 = 刷新页面
    const second = await boot(storage);
    expect(second.store.getState().design).toBe(GENERIC_THEME.id);
    second.store.getState().setPlatform({ osId: "windows" });
    expect(second.store.getState().theme.id).toBe(GENERIC_THEME.id);
  });

  it("存着的值非法时回落到「系统」，不是回落到某一套语言", async () => {
    for (const bad of ["strixmaid", "breeze", "Generic", "", "true"]) {
      const { store } = await boot(fakeStorage({ [DESIGN_KEY]: bad }));
      expect(store.getState().design, bad).toBe("system");
      store.getState().setPlatform({ osId: "windows" });
      expect(store.getState().theme.id, bad).toBe(FLUENT_THEME.id);
    }
  });

  it("没有 localStorage（隐私窗口、清过站点数据）时也是「系统」，且不抛错", async () => {
    const { store } = await boot(undefined);
    expect(store.getState().design).toBe("system");
    expect(() => store.getState().setDesign(GENERIC_THEME.id)).not.toThrow();
    expect(store.getState().theme.id).toBe(GENERIC_THEME.id);
  });
});
