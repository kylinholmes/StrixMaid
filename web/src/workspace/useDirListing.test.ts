import { describe, expect, it } from "vitest";
import { dirQueryKey } from "./useDirListing";

const sort = { key: "name", desc: false } as const;

describe("dirQueryKey", () => {
  it("路径、排序键、方向都进 key", () => {
    expect(dirQueryKey("/etc", sort, false)).not.toEqual(dirQueryKey("/var", sort, false));
    expect(dirQueryKey("/etc", sort, false)).not.toEqual(
      dirQueryKey("/etc", { key: "size", desc: false }, false),
    );
    expect(dirQueryKey("/etc", sort, false)).not.toEqual(
      dirQueryKey("/etc", { key: "name", desc: true }, false),
    );
  });

  it("show_hidden 也必须进 key", () => {
    // 过滤在服务端做，两个开关状态是两份不同的结果。不进 key 的话
    // react-query 会把上一次的结果当成命中，按一下开关列表纹丝不动。
    expect(dirQueryKey("/etc", sort, true)).not.toEqual(dirQueryKey("/etc", sort, false));
  });

  it("以 dir 开头，好让 refresh 能按前缀整目录失效", () => {
    expect(dirQueryKey("/etc", sort, true)[0]).toBe("dir");
    expect(dirQueryKey("/etc", sort, true)[1]).toBe("/etc");
  });
});
