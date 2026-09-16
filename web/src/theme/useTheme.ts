import { create } from "zustand";
import { type Distro, findDistro, UNKNOWN_DISTRO } from "./distro";
import {
  asDesign,
  type DesignPreference,
  type Mode,
  type PlatformIdentity,
  pickTheme,
  type Theme,
} from "./tokens";

interface ThemeState {
  mode: Mode;
  /** 设计语言偏好：用户选的那一档（「StrixMaid」或「系统」），与机器无关 */
  design: DesignPreference;
  /** 设计语言：偏好与机器一起决定的结果，界面实际用的就是它 */
  theme: Theme;
  /** 身份：当前会话所认证的那个节点是什么发行版。与 theme 是两件事 */
  distro: Distro;
  /**
   * 最近一次认出的机器身份。偏好改变时要拿它重算 `theme`，所以必须留着：
   * 只存算完的 `theme` 的话，切偏好时就没有东西可以重新判了。
   */
  identity: PlatformIdentity;
  setMode: (mode: Mode) => void;
  toggleMode: () => void;
  /** 换设计语言偏好。写盘、重算 theme、落 CSS 变量，三件事必须一起做 */
  setDesign: (design: DesignPreference) => void;
  /** 认出机器之后同时定下设计语言与身份。加维度时改 `PlatformIdentity`，不改这里 */
  setPlatform: (identity: PlatformIdentity) => void;
}

const STORAGE_KEY = "strixmaid.theme.mode";
const DESIGN_KEY = "strixmaid.theme.design";

function initialMode(): Mode {
  const saved = globalThis.localStorage?.getItem(STORAGE_KEY);
  if (saved === "dark" || saved === "light") return saved;
  // 未选择过则跟随系统；产品以暗色为主，所以取不到偏好时也用暗色
  return globalThis.matchMedia?.("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

/**
 * 读设计语言偏好。与 `initialMode` 同一套做法：localStorage 一个键，读不到就回落。
 *
 * 回落到「系统」而不是「StrixMaid」：没有存储（隐私窗口、清过站点数据、
 * 头一次打开）与显式选过「系统」应当是同一个结果，否则换台浏览器界面就变了样。
 * 合法值的判断交给 `asDesign`，非法值（手改过、旧版本留下的）一并回落。
 */
function initialDesign(): DesignPreference {
  return asDesign(globalThis.localStorage?.getItem(DESIGN_KEY));
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
 *
 * 发行版那档 accent **不看设计语言偏好**：它是「正在看哪台机器」的身份，
 * 选了 StrixMaid 的用户连上 Windows 仍然是蓝色 accent，只是中性色换成通用那套。
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

/** 启动时的偏好。只读一次盘，`design` 与初始 `theme` 必须同源。 */
const bootDesign = initialDesign();

export const useTheme = create<ThemeState>((set, get) => ({
  mode: initialMode(),
  design: bootDesign,
  identity: {},
  theme: pickTheme({}, bootDesign),
  distro: UNKNOWN_DISTRO,
  setMode: (mode) => {
    globalThis.localStorage?.setItem(STORAGE_KEY, mode);
    applyTheme(mode, get().theme, get().distro);
    set({ mode });
  },
  toggleMode: () => get().setMode(get().mode === "dark" ? "light" : "dark"),
  setDesign: (design) => {
    globalThis.localStorage?.setItem(DESIGN_KEY, design);
    const theme = pickTheme(get().identity, design);
    applyTheme(get().mode, theme, get().distro);
    set({ design, theme });
  },
  setPlatform: (identity) => {
    const theme = pickTheme(identity, get().design);
    const distro = findDistro(identity.osId);
    applyTheme(get().mode, theme, distro);
    set({ identity, theme, distro });
  },
}));
