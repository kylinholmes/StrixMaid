# 工作区 B 期 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development 或 superpowers:executing-plans 逐任务实现。步骤用 `- [ ]` 复选框跟踪。

**Goal:** 把 `/terminal` 与 `/files` 两个 StubPage 换成一个真正能用的工作区——开终端、敲命令、多标签、浏览目录、快速访问跳转。

**Architecture:** 一个工作区页 `/workspace`，文件区铺底 + 终端面板浮在其上（可拖高、可折叠）+ 左侧快速访问栏。终端字节流走独立的裸二进制 WS（不复用 `/ws` 控制面的 envelope 客户端），另写一个薄客户端 `termsocket.ts`。文件区、快速访问全走既有 REST。整个工作区的可变状态（标签页、当前 cwd、面板布局）收进一个 zustand store。

**Tech Stack:** React 19、react-router 7、zustand 5、@tanstack/react-query 5、@xterm/xterm + @xterm/addon-fit（本期新增）、openapi-fetch、CSS Modules。

**Spec:** `docs/roadmap/12-workspace.md`（B 期见 §3 表格与 §4.1–4.3、4.5、4.8）

## Global Constraints

- **纯前端。** 后端终端/文件/系统端点已完整，B 期不碰 Rust。C 期的 cwd 联动、D 期的图标/缩略图/分页不在本期。
- **终端裸二进制流不复用 `/ws` 客户端**（§4.3）：`web/src/lib/ws.ts` 是 envelope 多路复用，终端要另写薄客户端 `termsocket.ts`。
- **退出与断线必须分开显示**（§4.3，`/debug` 原型错在这里）：收到 `{"t":"exit"}` → 标 `已退出 (code N)` 并保留回看；WS 断了没收到 exit → `连接断开，PTY 仍在运行` 提供重连。
- **每会话上限 8 个，UI 自己算并在到达时禁用 `+`**（§4.3），不等后端 409。
- **Windows 驱动器根的 `name` 就是完整路径 `C:\`，不许与父路径拼接**（§4.5，`/debug` 原型栽在这里）：路径拼接统一走平台感知工具函数，不在组件里手写。
- **卡死的挂载不能凭空消失**（§4.2）：对「上一次见过、这次没了」的挂载点保留灰显条目并标「无响应」。
- **导航保留「终端」与「文件」两个入口，都指向工作区**（§4.1），只是初始状态不同。
- **刷新：焦点 + 手动两条，不做推送**（§4.8）。窗口重获焦点自动重取当前目录；固定位置的手动刷新按钮。刷新时保住滚动位置与选中项。
- **分页与虚拟滚动放 D**（§3）：B 用朴素列表即可；若自测发现大目录卡到影响使用再提前，本期先不做。
- CI 有 15 MiB 体积门槛（当前 release 10.5 MB）；xterm 打进前端资源，构建后确认 `size` job 不破。

---

## File Structure

```
web/src/workspace/
  Workspace.tsx        工作区页装配：快速访问栏 + 文件区 + 终端面板
  store.ts             zustand：标签页、活动标签、当前目录、面板布局、左栏折叠
  termsocket.ts        裸二进制 WS 薄客户端（连接、收发字节、resize、exit 帧、重连）
  TerminalPanel.tsx    终端面板：标签栏 + `+`（8 上限禁用）+ 拖高 + 折叠
  TerminalTab.tsx      单个终端：xterm 挂载 + termsocket 接线 + 退出/断线态
  FileList.tsx         文件只读列表：名称/大小/修改时间/权限，行导航
  QuickAccess.tsx      快速访问：已知文件夹 + 驱动器/挂载点（含无响应态）
  useDirListing.ts     取目录 + 焦点/手动刷新 + 保住滚动与选中
  path.ts              平台感知的路径拼接/取父目录（纯函数）
  path.test.ts         path.ts 单测
  store.test.ts        store 归约单测
  termsocket.test.ts   termsocket 用 mock WebSocket 的单测
  Workspace.module.css 工作区布局样式
```

修改：`web/src/app/App.tsx`（路由：`/workspace`，`/terminal`、`/files` 重定向到它并带初始态）、`web/src/app/pages.ts`（两个入口的 id 指向 workspace 或加初始态参数）、`web/package.json`（xterm 依赖）、`web/src/assets`（xterm CSS 内联或 import）。

---

## Task 1: 平台感知路径工具 `path.ts`

**Files:**
- Create: `web/src/workspace/path.ts`
- Test: `web/src/workspace/path.test.ts`

**Interfaces:**
- Produces:
  - `type Platform = "unix" | "windows"`
  - `platformOf(osId: string | undefined): Platform` —— `osId` 里含 `windows` → `"windows"`，否则 `"unix"`
  - `joinPath(parent: string, name: string, p: Platform): string`
  - `parentPath(path: string, p: Platform): string | null` —— 已在根返回 `null`
  - `isVirtualRoot(path: string, p: Platform): boolean` —— Windows 的 `\`（全部驱动器虚拟根）
  - `isDriveRoot(name: string, p: Platform): boolean` —— 形如 `C:\`

**关键约束**（§4.5、`providers/fs/windows.rs` 模块文档）：
- Windows 虚拟根 `\` 下的条目 `name` 是完整路径 `C:\`，`joinPath("\\", "C:\\", windows)` 必须返回 `C:\` 本身，**不拼成 `\C:\`**。
- Windows 普通目录用 `\` 分隔；Unix 用 `/`。
- `parentPath` 在 Windows 盘根 `C:\` 上返回虚拟根 `\`；在 `\` 上返回 `null`。Unix 在 `/` 上返回 `null`。

- [ ] **Step 1: 写失败测试**

```ts
import { describe, expect, it } from "vitest";
import { isDriveRoot, isVirtualRoot, joinPath, parentPath, platformOf } from "./path";

describe("platformOf", () => {
  it("识别 windows", () => expect(platformOf("windows")).toBe("windows"));
  it("其余当 unix", () => {
    expect(platformOf("ubuntu")).toBe("unix");
    expect(platformOf(undefined)).toBe("unix");
  });
});

describe("joinPath unix", () => {
  it("普通拼接", () => expect(joinPath("/etc", "hosts", "unix")).toBe("/etc/hosts"));
  it("根下拼接不出现双斜杠", () => expect(joinPath("/", "etc", "unix")).toBe("/etc"));
});

describe("joinPath windows", () => {
  it("普通目录用反斜杠", () =>
    expect(joinPath("C:\\Users", "kylin", "windows")).toBe("C:\\Users\\kylin"));
  it("虚拟根下的驱动器名就是完整路径，不拼接", () =>
    expect(joinPath("\\", "C:\\", "windows")).toBe("C:\\"));
  it("盘根下拼接", () =>
    expect(joinPath("C:\\", "Windows", "windows")).toBe("C:\\Windows"));
});

describe("parentPath", () => {
  it("unix 根返回 null", () => expect(parentPath("/", "unix")).toBeNull());
  it("unix 普通", () => expect(parentPath("/etc/nginx", "unix")).toBe("/etc"));
  it("windows 盘根回到虚拟根", () => expect(parentPath("C:\\", "windows")).toBe("\\"));
  it("windows 虚拟根返回 null", () => expect(parentPath("\\", "windows")).toBeNull());
  it("windows 普通", () => expect(parentPath("C:\\Users\\kylin", "windows")).toBe("C:\\Users"));
});

describe("root 判定", () => {
  it("虚拟根", () => expect(isVirtualRoot("\\", "windows")).toBe(true));
  it("驱动器根", () => expect(isDriveRoot("C:\\", "windows")).toBe(true));
  it("普通名不是驱动器根", () => expect(isDriveRoot("Users", "windows")).toBe(false));
});
```

- [ ] **Step 2: 跑测试确认失败** — `cd web && bun run test path` → FAIL（模块不存在）
- [ ] **Step 3: 实现 `path.ts`**（按上述接口与约束，纯字符串处理）
- [ ] **Step 4: 跑测试确认通过** — `bun run test path` → PASS
- [ ] **Step 5: 提交** — `feat(web): 平台感知路径工具（B 期地基）`

---

## Task 2: 引入 xterm 依赖

**Files:**
- Modify: `web/package.json`

- [ ] **Step 1: 装依赖** — `cd web && bun add @xterm/xterm @xterm/addon-fit`
- [ ] **Step 2: 确认锁定** — 检查 `package.json` 出现 `@xterm/xterm`、`@xterm/addon-fit`，`bun.lock` 更新
- [ ] **Step 3: 构建确认体积** — `bun run build`，看 `dist` 产物；记下增量，确认远小于 15 MiB 门槛
- [ ] **Step 4: 提交** — `build(web): 引入 xterm.js`

---

## Task 3: 终端裸二进制 WS 薄客户端 `termsocket.ts`

**Files:**
- Create: `web/src/workspace/termsocket.ts`
- Test: `web/src/workspace/termsocket.test.ts`

**Interfaces:**
- Consumes: session token（`localStorage["strixmaid.session.token"]`）
- Produces:
  - `interface TermSocketHandlers { onData(bytes: Uint8Array): void; onExit(frame: ExitFrame): void; onStatus(up: boolean): void }`
  - `interface ExitFrame { reason: string; code?: number; signal?: number }`
  - `class TermSocket { constructor(id: string, h: TermSocketHandlers); send(bytes: Uint8Array): void; resize(cols: number, rows: number): void; close(): void }`

**协议**（`ws/terminal.rs` 模块文档）：
- URL `${ws|wss}://${host}/ws/terminal/{id}`，子协议 `["bearer", token]`。
- 二进制帧 = PTY 原始字节，双向透传。
- 客户端→服务端文本 `{"t":"resize","cols":N,"rows":M}`。
- 服务端→客户端文本 `{"t":"exit","reason":…,"code"?:…,"signal"?:…}` 随后关闭连接。
- **exit 与断线分开**：收到 exit 帧 → `onExit`；WS onclose 且未收到 exit → `onStatus(false)`（可重连），不当作退出。
- `binaryType = "arraybuffer"`。

- [ ] **Step 1: 写失败测试**（mock WebSocket）

```ts
import { beforeEach, describe, expect, it, vi } from "vitest";
import { TermSocket } from "./termsocket";

class MockWS {
  static last: MockWS | null = null;
  onopen: (() => void) | null = null;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  sent: unknown[] = [];
  binaryType = "";
  readyState = 1;
  constructor(public url: string, public protocols?: string[]) { MockWS.last = this; }
  send(d: unknown) { this.sent.push(d); }
  close() { this.readyState = 3; this.onclose?.(); }
}

describe("TermSocket", () => {
  beforeEach(() => {
    vi.stubGlobal("WebSocket", MockWS as unknown as typeof WebSocket);
    globalThis.localStorage?.setItem("strixmaid.session.token", "tok");
  });

  it("连接带 bearer 子协议与终端 id", () => {
    new TermSocket("abc", { onData() {}, onExit() {}, onStatus() {} });
    expect(MockWS.last?.url).toContain("/ws/terminal/abc");
    expect(MockWS.last?.protocols).toEqual(["bearer", "tok"]);
  });

  it("二进制帧交给 onData", () => {
    const seen: Uint8Array[] = [];
    new TermSocket("abc", { onData: (b) => seen.push(b), onExit() {}, onStatus() {} });
    MockWS.last?.onmessage?.({ data: new TextEncoder().encode("hi").buffer });
    expect(new TextDecoder().decode(seen[0])).toBe("hi");
  });

  it("exit 文本帧交给 onExit，不当断线", () => {
    let exit: unknown = null;
    let up: boolean | null = null;
    new TermSocket("abc", { onData() {}, onExit: (e) => { exit = e; }, onStatus: (u) => { up = u; } });
    MockWS.last?.onmessage?.({ data: JSON.stringify({ t: "exit", reason: "exited", code: 42 }) });
    expect(exit).toEqual({ reason: "exited", code: 42 });
  });

  it("resize 发出 JSON 文本帧", () => {
    const ts = new TermSocket("abc", { onData() {}, onExit() {}, onStatus() {} });
    MockWS.last!.readyState = 1;
    ts.resize(120, 32);
    expect(MockWS.last?.sent).toContainEqual(JSON.stringify({ t: "resize", cols: 120, rows: 32 }));
  });
});
```

- [ ] **Step 2: 跑测试确认失败** — `bun run test termsocket` → FAIL
- [ ] **Step 3: 实现 `termsocket.ts`**（含 onclose 时若未收 exit 则 `onStatus(false)`；`send` 用 `readyState===OPEN` 守卫）
- [ ] **Step 4: 跑测试确认通过** — PASS
- [ ] **Step 5: 提交** — `feat(web): 终端裸二进制 WS 薄客户端`

---

## Task 4: 工作区状态 store

**Files:**
- Create: `web/src/workspace/store.ts`
- Test: `web/src/workspace/store.test.ts`

**Interfaces:**
- Produces（zustand store）:
  - `interface Tab { id: string; title: string; status: "live" | "disconnected" | "exited"; exitLabel?: string }`
  - state: `tabs: Tab[]; activeId: string | null; cwd: string | null; panelHeight: number; panelCollapsed: boolean; railCollapsed: boolean`
  - actions: `addTab(t: Tab)`, `removeTab(id)`, `setActive(id)`, `markExited(id, label)`, `markDisconnected(id)`, `markLive(id)`, `setCwd(path)`, `setPanelHeight(px)`, `togglePanel()`, `toggleRail()`
  - selector helper: `MAX_TABS = 8`，`atTabLimit(s): boolean`（`tabs.length >= MAX_TABS`）
  - `removeTab` 删的是活动标签时，`activeId` 落到相邻标签（优先右、否则左、都无则 `null`）。

- [ ] **Step 1: 写失败测试**

```ts
import { beforeEach } from "vitest";
import { describe, expect, it } from "vitest";
import { atTabLimit, MAX_TABS, useWorkspace } from "./store";

const reset = () => useWorkspace.setState({ tabs: [], activeId: null, cwd: null });

describe("workspace store", () => {
  beforeEach(reset);

  it("加标签自动成为活动", () => {
    useWorkspace.getState().addTab({ id: "a", title: "sh", status: "live" });
    expect(useWorkspace.getState().activeId).toBe("a");
  });

  it("关活动标签落到相邻（优先右）", () => {
    const s = useWorkspace.getState();
    s.addTab({ id: "a", title: "sh", status: "live" });
    s.addTab({ id: "b", title: "sh", status: "live" });
    s.addTab({ id: "c", title: "sh", status: "live" });
    useWorkspace.getState().setActive("b");
    useWorkspace.getState().removeTab("b");
    expect(useWorkspace.getState().activeId).toBe("c");
  });

  it("退出态保留标签，只改状态与标签文字", () => {
    useWorkspace.getState().addTab({ id: "a", title: "sh", status: "live" });
    useWorkspace.getState().markExited("a", "已退出 (code 42)");
    const tab = useWorkspace.getState().tabs.find((t) => t.id === "a");
    expect(tab?.status).toBe("exited");
    expect(tab?.exitLabel).toBe("已退出 (code 42)");
  });

  it("上限判定", () => {
    for (let i = 0; i < MAX_TABS; i++)
      useWorkspace.getState().addTab({ id: String(i), title: "sh", status: "live" });
    expect(atTabLimit(useWorkspace.getState())).toBe(true);
  });
});
```

- [ ] **Step 2: 跑测试确认失败** → FAIL
- [ ] **Step 3: 实现 `store.ts`**
- [ ] **Step 4: 跑测试确认通过** → PASS
- [ ] **Step 5: 提交** — `feat(web): 工作区状态 store`

---

## Task 5: 文件只读列表 `FileList.tsx` + 取数 `useDirListing.ts`

**Files:**
- Create: `web/src/workspace/FileList.tsx`, `web/src/workspace/useDirListing.ts`
- Modify: `web/src/workspace/Workspace.module.css`（列表样式）

**Interfaces:**
- Consumes: `api.GET("/api/v1/files", { params: { query: { path } } })` → `DirListing`；`joinPath`/`parentPath`（Task 1）；`platformOf` 取自 `capabilitiesQuery().identity.os_id`。
- Produces:
  - `useDirListing(path: string | null): { data?: DirListing; isLoading; error; refresh() }` —— react-query，`queryKey: ["dir", path]`；封装焦点刷新（`visibilitychange`/`focus` → `refresh`）与手动 `refresh`。
  - `<FileList path cwd onEnter={(entry) => void} />` —— 列：名称（带 kind 前缀符号）、大小（`fmtBytes`，目录显示 `—`）、修改时间、权限（八进制）。目录/符号链接可点进；点目录 → `onEnter(joinPath(...))`。顶部一行：`← → ↑` + 当前路径 + 手动刷新按钮。

**约束：** 目录条目导航用 `joinPath`（Task 1），驱动器根条目直接用 `entry.name`（§4.5）。刷新保住滚动位置（记住 `scrollTop`，数据更新后恢复）与选中项。

- [ ] **Step 1: 浏览器手动验证准备** —— 本任务以真实交互为主，先写 `useDirListing` 的纯逻辑（焦点监听装/卸）可加一个轻量测试；列表渲染走浏览器验证。
- [ ] **Step 2: 实现 `useDirListing.ts`**（react-query + focus/visibilitychange 监听 + 手动 refresh；`enabled: path !== null`）
- [ ] **Step 3: 实现 `FileList.tsx`**（列表 + 面包屑行 + 上一级/刷新按钮；用既有 `Table`/`fmt` 组件与 CSS Module）
- [ ] **Step 4: typecheck + biome** — `bun run typecheck && bun run check`
- [ ] **Step 5: 提交** — `feat(web): 文件只读列表与目录取数`

---

## Task 6: 快速访问 `QuickAccess.tsx`

**Files:**
- Create: `web/src/workspace/QuickAccess.tsx`

**Interfaces:**
- Consumes: `capabilitiesQuery()`（拿 `identity.os_id` 判平台、拿 home）、`api.GET("/api/v1/system/info")` → `filesystems`。
- Produces: `<QuickAccess current={cwd} onGo={(path) => void} />`
  - 两段：**已知文件夹**（Unix：`$HOME` 与 `/`；Windows：主目录/下载/文档/桌面——B 期可先只放主目录与 `/`(`\`)，已知文件夹的精确清单可后续补）；**驱动器与挂载点**（读 `filesystems`，每条一个 `mount_point`，显示占用条）。
  - **无响应挂载**（§4.2）：store 一个「上次见过的挂载点集合」；本次 `filesystems` 里缺失的，保留灰显条目标「无响应」，不让它凭空消失。

**约束：** Windows 驱动器 `mount_point` 就是 `C:\`，点它直接跳该路径，不拼接。

- [ ] **Step 1: 实现 `QuickAccess.tsx`**（含无响应挂载的「上次见过」逻辑，可用 `useRef` 存集合）
- [ ] **Step 2: typecheck + biome**
- [ ] **Step 3: 提交** — `feat(web): 快速访问栏（已知文件夹 + 挂载点，含无响应态）`

---

## Task 7: 终端标签 `TerminalTab.tsx`

**Files:**
- Create: `web/src/workspace/TerminalTab.tsx`

**Interfaces:**
- Consumes: `TermSocket`（Task 3）、`@xterm/xterm`、`@xterm/addon-fit`、workspace store 的 `markExited/markDisconnected/markLive`。
- Produces: `<TerminalTab id={string} active={boolean} />`
  - 挂载 xterm 到一个 div，装 FitAddon；`ResizeObserver` 或窗口 resize 时 `fit()` 后 `termsocket.resize(cols, rows)`。
  - `term.onData` → `termsocket.send`；`termsocket.onData` → `term.write`。
  - `onExit` → `markExited(id, exitLabel(frame))`，**保留 xterm 内容**（不 dispose），显示一条「已退出」横幅。
  - `onStatus(false)` 且未退出 → `markDisconnected(id)`，显示「连接断开，PTY 仍在运行」+ 重连按钮（重连即重建 `TermSocket`，附着会回放回看缓冲）。
  - 非活动标签**保持连接不断**（§4.3），只用 CSS 隐藏，不卸载。
  - `exitLabel(frame)`: 有 `code` → `已退出 (code N)`；有 `signal` → `已退出 (signal N)`；否则 `已退出 (${reason})`。

- [ ] **Step 1: 实现 `TerminalTab.tsx`**（xterm 生命周期用 `useEffect`，dispose 在卸载时；socket 在 `onExit` 后不自动重连）
- [ ] **Step 2: typecheck + biome**
- [ ] **Step 3: 提交** — `feat(web): 终端标签（xterm + 退出/断线分显）`

---

## Task 8: 终端面板 `TerminalPanel.tsx`

**Files:**
- Create: `web/src/workspace/TerminalPanel.tsx`

**Interfaces:**
- Consumes: workspace store、`api.POST("/api/v1/terminals")`（开）、`api.DELETE("/api/v1/terminals/{id}")`（关）、`TerminalTab`（Task 7）、`atTabLimit`（Task 4）。
- Produces: `<TerminalPanel />`
  - 标签栏：每个标签显示 title/状态；`×` 关闭（对 live 标签发 DELETE，对 exited 标签只从 store 移除）；`+` 新建（POST /terminals 无 shell 参数 → 默认登录 shell，拿到 id 后 `addTab`）。
  - **`+` 在 `atTabLimit` 时禁用**（§4.3），不等 409。
  - 上边缘可拖动调高度（改 store `panelHeight`）；整体可折叠成一条底栏（`togglePanel`）。
  - 面板**浮在文件区之上**，不挤压文件区（绝对定位 / grid overlay）。

> shell 选择下拉（§4.3 的 `+` 旁下拉 + `GET /terminals/shells`）**本期暂缓**：`+` 直接开默认 shell。理由：该端点要新增 worker RPC（登录 shell 是 per-user，只有 worker 知道），属后端改动，与「B 期纯前端」冲突；且 §7 验收不要求下拉。记入 PR 描述作为 B 内延后项。

- [ ] **Step 1: 实现 `TerminalPanel.tsx`**（拖高用指针事件改 store height；开/关接 REST）
- [ ] **Step 2: typecheck + biome**
- [ ] **Step 3: 提交** — `feat(web): 终端面板（多标签、8 上限禁用、拖高折叠）`

---

## Task 9: 工作区装配 + 路由

**Files:**
- Create: `web/src/workspace/Workspace.tsx`, `web/src/workspace/Workspace.module.css`
- Modify: `web/src/app/App.tsx`, `web/src/app/pages.ts`

**Interfaces:**
- Consumes: `QuickAccess`、`FileList`、`TerminalPanel`、workspace store。
- Produces: `<Workspace initial="terminal" | "files" />`
  - 布局：左快速访问栏（可折叠）+ 右侧文件区铺底 + 终端面板浮层。
  - 从「终端」入口进 → 终端面板展开；从「文件」入口进 → 面板收起。用路由或 query 区分初始态。
  - 文件区 `onEnter` 改 store `cwd`（同时 FileList 的 path 跟随）；`cwd` 初始为 home（从 capabilities）。

**路由改动（App.tsx）：**
```tsx
<Route path="/workspace" element={<Workspace initial="terminal" />} />
<Route path="/terminal" element={<Workspace initial="terminal" />} />
<Route path="/files" element={<Workspace initial="files" />} />
```
（保留两个入口，都渲染 Workspace，只是 `initial` 不同——§4.1。`pages.ts` 的两个条目 id 保持 `terminal`/`files`，Rail 里点击各自导航到 `/terminal`、`/files`。）

- [ ] **Step 1: 实现 `Workspace.tsx` + CSS**（grid：`[rail] [main]`，main 内文件区 + 浮层面板）
- [ ] **Step 2: 改 App.tsx 路由**（`/terminal`、`/files` → Workspace，删对应两行 StubPage）
- [ ] **Step 3: typecheck + biome + 构建** — `bun run typecheck && bun run check && bun run build`
- [ ] **Step 4: 提交** — `feat(web): 工作区页装配与路由`

---

## Task 10: 浏览器端到端自测（§6 第 6 项）

**约束：** 走 Orca 内嵌浏览器预览（见项目记忆），不用 computer-use/Chrome 扩展。需要真后端在跑（本机开发要给 `helper_path` 绝对路径，否则登录页报「认证组件不可用」，见交接文档 §8）。

- [ ] **Step 1: 起后端 + 前端 dev**（`cargo run` 服务端 + `bun run dev`，或直接构建后由服务端 embed 提供）
- [ ] **Step 2: 登录 → 进工作区 → 开终端 → 敲 `echo hi` 看回显 → 开第二个标签 → 切换 → `exit 42` 看「已退出 (code 42)」且保留内容**
- [ ] **Step 3: 浏览目录：从快速访问进主目录 → 点进子目录 → 上一级 → 在终端 `mkdir t1` 后切窗口回来点手动刷新，新目录出现且滚动位置未跳顶**
- [ ] **Step 4: 到 8 个标签时 `+` 禁用**
- [ ] **Step 5: 修掉自测暴露的问题，最终 `bun run test && bun run check && bun run build` 全绿**
- [ ] **Step 6: 提交** — `test(web): 工作区端到端自测修正`

---

## Self-Review

**Spec coverage（§ 对照）：**
- §4.1 页面形态（两入口、浮层面板、可拖高折叠、左栏折叠）→ Task 8、9
- §4.2 快速访问（已知文件夹 + 挂载点 + 无响应态）→ Task 6
- §4.3 终端（多标签、退出/断线分显、8 上限 UI 禁用、裸二进制客户端）→ Task 3、7、8
- §4.5 文件区列表视图 + 路径拼接工具 + 驱动器根不拼接 → Task 1、5
- §4.8 焦点 + 手动刷新，保住滚动与选中 → Task 5
- §6 测试：路径拼接（Task 1）、大目录（延后到 D，已在约束说明）、Windows 驱动器根（Task 1 覆盖逻辑）、浏览器实测（Task 10）
- §7 验收：多标签✓、退出/断线分显✓、8 上限禁用✓、两种视图——**仅列表视图**（平铺图标是 D 期），快速访问含挂载点✓、无响应态✓、mkdir 后焦点刷新✓
- **cwd 双向联动（§4.4）不在 B**（C 期）；**平铺图标/缩略图/分页（§4.7、4.5 虚拟滚动）不在 B**（D 期）——与 §3 分期一致。

**延后项（记入 PR 描述）：** shell 选择下拉 + `GET /terminals/shells`（需后端 RPC，B 保持纯前端）；平铺图标视图、缩略图、分页/虚拟滚动（D 期）；cwd 联动（C 期）。

**Type consistency：** `Tab.status` 用 `"live"|"disconnected"|"exited"` 贯穿 store（Task 4）与 TerminalTab（Task 7）；`ExitFrame` 字段 `reason/code?/signal?` 贯穿 termsocket（Task 3）与 exitLabel（Task 7）；`Platform` 与 `joinPath/parentPath` 签名贯穿 Task 1、5、6。
