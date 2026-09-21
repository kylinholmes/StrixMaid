import { describe, expect, it } from "vitest";
import { sideButtonAction } from "./mousenav";

describe("sideButtonAction", () => {
  it("第四键（button 3）是后退", () => expect(sideButtonAction(3)).toBe("back"));
  it("第五键（button 4）是前进", () => expect(sideButtonAction(4)).toBe("forward"));

  it("左中右三键不参与导航", () => {
    expect(sideButtonAction(0)).toBeNull();
    expect(sideButtonAction(1)).toBeNull();
    expect(sideButtonAction(2)).toBeNull();
  });

  it("更多的键（有些鼠标有第六键）不认", () => expect(sideButtonAction(5)).toBeNull());
});
