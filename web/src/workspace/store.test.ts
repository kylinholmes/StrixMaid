import { beforeEach, describe, expect, it } from "vitest";
import { atTabLimit, MAX_TABS, useWorkspace } from "./store";

const reset = () => useWorkspace.setState({ tabs: [], activeId: null, cwd: null });

describe("workspace store", () => {
  beforeEach(reset);

  it("加标签自动成为活动", () => {
    useWorkspace.getState().addTab({ id: "a", title: "sh", status: "live" });
    expect(useWorkspace.getState().activeId).toBe("a");
  });

  it("关活动标签落到相邻（优先右，右没有落左）", () => {
    const add = (id: string) => useWorkspace.getState().addTab({ id, title: "sh", status: "live" });
    add("a");
    add("b");
    add("c");
    useWorkspace.getState().setActive("b");
    useWorkspace.getState().removeTab("b");
    expect(useWorkspace.getState().activeId).toBe("c");
    useWorkspace.getState().removeTab("c");
    expect(useWorkspace.getState().activeId).toBe("a");
    useWorkspace.getState().removeTab("a");
    expect(useWorkspace.getState().activeId).toBeNull();
  });

  it("关非活动标签不动活动指针", () => {
    const add = (id: string) => useWorkspace.getState().addTab({ id, title: "sh", status: "live" });
    add("a");
    add("b");
    useWorkspace.getState().removeTab("a");
    expect(useWorkspace.getState().activeId).toBe("b");
  });

  it("退出态保留标签，只改状态与标签文字", () => {
    useWorkspace.getState().addTab({ id: "a", title: "sh", status: "live" });
    useWorkspace.getState().markExited("a", "已退出 (code 42)");
    const tab = useWorkspace.getState().tabs.find((t) => t.id === "a");
    expect(tab?.status).toBe("exited");
    expect(tab?.exitLabel).toBe("已退出 (code 42)");
    expect(useWorkspace.getState().tabs).toHaveLength(1);
  });

  it("断线与恢复", () => {
    useWorkspace.getState().addTab({ id: "a", title: "sh", status: "live" });
    useWorkspace.getState().markDisconnected("a");
    expect(useWorkspace.getState().tabs[0]?.status).toBe("disconnected");
    useWorkspace.getState().markLive("a");
    expect(useWorkspace.getState().tabs[0]?.status).toBe("live");
  });

  it("已退出的标签不被断线/恢复改写", () => {
    // WS 在 exit 帧之后总会跟一个 close；顺序颠倒或迟到的事件不能把
    // 「已退出」改写回「断线可重连」——那正是 /debug 原型犯的错。
    useWorkspace.getState().addTab({ id: "a", title: "sh", status: "live" });
    useWorkspace.getState().markExited("a", "已退出 (code 1)");
    useWorkspace.getState().markDisconnected("a");
    expect(useWorkspace.getState().tabs[0]?.status).toBe("exited");
    useWorkspace.getState().markLive("a");
    expect(useWorkspace.getState().tabs[0]?.status).toBe("exited");
  });

  it("上限判定", () => {
    for (let i = 0; i < MAX_TABS; i++)
      useWorkspace.getState().addTab({ id: String(i), title: "sh", status: "live" });
    expect(atTabLimit(useWorkspace.getState())).toBe(true);
    useWorkspace.getState().removeTab("0");
    expect(atTabLimit(useWorkspace.getState())).toBe(false);
  });

  it("已退出的标签不占上限名额——服务端早已释放", () => {
    for (let i = 0; i < MAX_TABS; i++)
      useWorkspace.getState().addTab({ id: String(i), title: "sh", status: "live" });
    expect(atTabLimit(useWorkspace.getState())).toBe(true);
    useWorkspace.getState().markExited("0", "已退出 (code 0)");
    expect(atTabLimit(useWorkspace.getState())).toBe(false);
    expect(useWorkspace.getState().tabs).toHaveLength(MAX_TABS);
  });
});
