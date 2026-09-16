import { create } from "zustand";
import { type Distro, findDistro, UNKNOWN_DISTRO } from "./distro";
import { type Mode, type PlatformIdentity, pickTheme, type Theme } from "./tokens";

interface ThemeState {
  mode: Mode;
  /** 设计语言：这台机器的界面该长什么样 */
  theme: Theme;
  /** 身份：当前会话所认证的那个节点是什么发行版。与 theme 是两件事 */
  distro: Distro;
  setMode: (mode: Mode) => void;
  toggleMode: () => void;
  /** 认出机器之后同时定下设计语言与身份。加维度时改 `PlatformIdentity`，不改这里 */
  setPlatform: (identity: PlatformIdentity) => void;
}

const STORAGE_KEY = "strixmaid.theme.mode";

function initialMode(): Mode {
  const saved = globalThis.localStorage?.getItem(STORAGE_KEY);
  if (saved === "dark" || saved === "light") return saved;
  // 未选择过则跟随系统；产品以暗色为主，所以取不到偏好时也用暗色
  return globalThis.matchMedia?.("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

/**
 * 把主题写进 :root。
 *
 * 中性色整套来自选中的设计语言（tokens.ts），值已经是最终的 CSS 颜色串，
 * 这里不再做任何换算。数据色是静态的，写在 tokens.css 里。
 *
 * `--accent` 分两层：先落主题自带的那档，认得出发行版再用发行版自己的盖掉。
 * 认不出时不盖，留主题那档——通用主题那档与「认不出」的灰同值，
 * 所以对今天的 Linux / macOS 来说结果与第二版逐字相同；
 * 将来若出现「认得出桌面环境、认不出发行版」的机器，拿到的会是桌面环境的主题色，
 * 而不是一律回落成灰。
 */
export function applyTheme(mode: Mode, theme: Theme, distro: Distro): void {
  const root = document.documentElement;
  for (const [token, value] of Object.entries(theme.tokens[mode])) {
    root.style.setProperty(`--${token}`, value);
  }
  if (distro.id !== UNKNOWN_DISTRO.id) {
    root.style.setProperty("--accent", distro.accent[mode]);
  }
  root.dataset.mode = mode;
  root.style.colorScheme = mode;
}

export const useTheme = create<ThemeState>((set, get) => ({
  mode: initialMode(),
  theme: pickTheme({}),
  distro: UNKNOWN_DISTRO,
  setMode: (mode) => {
    globalThis.localStorage?.setItem(STORAGE_KEY, mode);
    applyTheme(mode, get().theme, get().distro);
    set({ mode });
  },
  toggleMode: () => get().setMode(get().mode === "dark" ? "light" : "dark"),
  setPlatform: (identity) => {
    const theme = pickTheme(identity);
    const distro = findDistro(identity.osId);
    applyTheme(get().mode, theme, distro);
    set({ theme, distro });
  },
}));
