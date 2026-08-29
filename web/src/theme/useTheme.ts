import { create } from "zustand";
import { type Distro, findDistro, UNKNOWN_DISTRO } from "./distro";
import { hueOf, oklch } from "./oklch";
import { CHROMA, chromaFor, type Mode, RAMP } from "./tokens";

interface ThemeState {
  mode: Mode;
  /** 当前会话所认证的那个节点的发行版（spec §2.2 多节点一节） */
  distro: Distro;
  setMode: (mode: Mode) => void;
  toggleMode: () => void;
  setDistroById: (id: string | null | undefined) => void;
}

const STORAGE_KEY = "strixmaid.theme.mode";

function initialMode(): Mode {
  const saved = globalThis.localStorage?.getItem(STORAGE_KEY);
  if (saved === "dark" || saved === "light") return saved;
  // 未选择过则跟随系统；产品以暗色为主，所以取不到偏好时也用暗色
  return globalThis.matchMedia?.("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

/**
 * 把中性色写进 :root。
 *
 * 中性色的色相来自发行版，所以它不是一组静态常量，必须在运行时算。
 * 数据色（五资源 + 三状态）是静态的，写在 tokens.css 里，不经过这里。
 */
export function applyTheme(mode: Mode, distro: Distro): void {
  const root = document.documentElement;
  const hue = distro.brand ? hueOf(distro.brand) : 0;
  const base = distro.brand ? CHROMA[mode] : 0;

  for (const [token, lightness] of Object.entries(RAMP[mode])) {
    root.style.setProperty(`--${token}`, oklch(lightness, chromaFor(token, base, mode), hue));
  }
  root.dataset.mode = mode;
  root.style.colorScheme = mode;
}

export const useTheme = create<ThemeState>((set, get) => ({
  mode: initialMode(),
  distro: UNKNOWN_DISTRO,
  setMode: (mode) => {
    globalThis.localStorage?.setItem(STORAGE_KEY, mode);
    applyTheme(mode, get().distro);
    set({ mode });
  },
  toggleMode: () => get().setMode(get().mode === "dark" ? "light" : "dark"),
  setDistroById: (id) => {
    const distro = findDistro(id);
    applyTheme(get().mode, distro);
    set({ distro });
  },
}));
