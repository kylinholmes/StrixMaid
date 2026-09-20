import { create } from "zustand";

/**
 * 工作区的可变状态（`docs/roadmap/12-workspace.md` §4.1、§4.3）。
 *
 * 标签页、当前目录、面板布局收在一个 store 里而不是散在组件 state：
 * 终端面板与文件区是同一个页面的两半，「切走标签保持连接」「从文件入口进来
 * 面板收起」这类状态在组件卸载后还要活着。
 */

/** 一个终端标签页。 */
export interface Tab {
  /** 终端 id（`POST /terminals` 返回的）。 */
  id: string;
  /** 标签显示名（shell 文件名）。 */
  title: string;
  /**
   * - `live`：WS 连着，正常收发；
   * - `disconnected`：WS 断了但**没收到 exit 帧**——PTY 还在跑，可重连（§4.3）；
   * - `exited`：终端本身没了。保留回看内容，由人手动关标签。
   */
  status: "live" | "disconnected" | "exited";
  /** `exited` 时的展示文字，如 `已退出 (code 42)`。 */
  exitLabel?: string;
  /** 实际启动的 shell 路径（cwd 联动要按方言组 `cd` 命令、判可不可轮询）。 */
  shell?: string;
  /** worker 内 shell 的 pid（cwd 兜底轮询的目标）。 */
  pid?: number;
  /** 这个终端报出的当前目录（OSC 7 或轮询）。不知道就没有——宁可空着不可指错。 */
  cwd?: string;
  /** 收到过 OSC 7。此后轮询永久停用：shell 亲口说的比进程属性可信。 */
  osc7?: boolean;
}

/** 每会话终端上限（`TerminalConfig::max_per_session` 的默认值）。UI 自己算，不等后端 409。 */
export const MAX_TABS = 8;

interface WorkspaceState {
  tabs: Tab[];
  activeId: string | null;
  /** 文件区当前目录；`null` 表示还没定位（等 capabilities 给出主目录）。 */
  cwd: string | null;
  /** 终端面板高度（px）。 */
  panelHeight: number;
  /** 终端面板是否折叠成底栏。 */
  panelCollapsed: boolean;
  /** 左侧快速访问栏是否折叠。 */
  railCollapsed: boolean;
  /** 隐藏以 `.` 开头的条目（dotfile 约定，三平台一致处理）。 */
  hideHidden: boolean;

  addTab(tab: Tab): void;
  removeTab(id: string): void;
  setActive(id: string): void;
  markExited(id: string, label: string): void;
  markDisconnected(id: string): void;
  markLive(id: string): void;
  setCwd(path: string): void;
  /** 某个终端报出了自己的 cwd（`viaOsc7` 标记来源；OSC 7 一旦出现即压过轮询）。 */
  setTabCwd(id: string, path: string, viaOsc7: boolean): void;
  setPanelHeight(px: number): void;
  setPanelCollapsed(collapsed: boolean): void;
  toggleRail(): void;
  toggleHidden(): void;
}

/**
 * 到达每会话上限即禁用「+」（§4.3）。
 *
 * **只数还活着的**（含断线——PTY 仍在跑、仍占服务端名额）：已退出的终端
 * 服务端已经释放名额，标签只是留着给人看最后的输出，不该挡住新终端。
 */
export function atTabLimit(s: Pick<WorkspaceState, "tabs">): boolean {
  return s.tabs.filter((t) => t.status !== "exited").length >= MAX_TABS;
}

/** 已退出是终末态：迟到的断线/恢复事件不得改写它（exit 帧之后 WS 总会跟一个 close）。 */
function transition(tabs: Tab[], id: string, next: Partial<Tab>): Tab[] {
  return tabs.map((t) => (t.id === id && t.status !== "exited" ? { ...t, ...next } : t));
}

export const useWorkspace = create<WorkspaceState>((set) => ({
  tabs: [],
  activeId: null,
  cwd: null,
  panelHeight: 320,
  panelCollapsed: false,
  railCollapsed: false,
  hideHidden: false,

  addTab: (tab) => set((s) => ({ tabs: [...s.tabs, tab], activeId: tab.id })),

  removeTab: (id) =>
    set((s) => {
      const idx = s.tabs.findIndex((t) => t.id === id);
      const tabs = s.tabs.filter((t) => t.id !== id);
      let activeId = s.activeId;
      if (s.activeId === id) {
        // 落到相邻：优先右（原下标处现在是右邻），否则左，都没有就空。
        const next = tabs[idx] ?? tabs[idx - 1] ?? null;
        activeId = next?.id ?? null;
      }
      return { tabs, activeId };
    }),

  setActive: (id) => set({ activeId: id }),

  markExited: (id, label) =>
    set((s) => ({
      tabs: s.tabs.map((t) => (t.id === id ? { ...t, status: "exited", exitLabel: label } : t)),
    })),

  markDisconnected: (id) =>
    set((s) => ({ tabs: transition(s.tabs, id, { status: "disconnected" }) })),

  markLive: (id) => set((s) => ({ tabs: transition(s.tabs, id, { status: "live" }) })),

  setCwd: (path) => set({ cwd: path }),

  setTabCwd: (id, path, viaOsc7) =>
    set((s) => ({
      tabs: transition(s.tabs, id, viaOsc7 ? { cwd: path, osc7: true } : { cwd: path }),
    })),
  // 上限留出 160px 给文件区工具栏：面板长过容器会把自己的拖把手和标签栏
  // 顶出可视区，只剩一个看不见的焦点元素能缩回来。
  setPanelHeight: (px) =>
    set({
      panelHeight: Math.min(Math.max(120, px), Math.max(200, window.innerHeight - 160)),
    }),
  setPanelCollapsed: (collapsed) => set({ panelCollapsed: collapsed }),
  toggleRail: () => set((s) => ({ railCollapsed: !s.railCollapsed })),
  toggleHidden: () => set((s) => ({ hideHidden: !s.hideHidden })),
}));
