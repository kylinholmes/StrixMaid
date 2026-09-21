/**
 * 鼠标侧键 → 文件区的前进 / 后退。
 *
 * # 为什么必须 `preventDefault`
 *
 * Chrome 与 Edge 在 Windows 上把侧键**默认**当成浏览器的前进后退。文件区的
 * 历史是自己的一套（`Workspace.tsx` 里的 `past` / `future`），不在 React
 * Router 的历史里；不拦默认行为的话，按一下侧键会把整个 SPA 退到上一个
 * 路由（比如概览页），而不是在文件区退一层目录——两件事同时发生，
 * 而且后者根本看不见。
 *
 * 拦截要挂在 `mousedown` 上：浏览器的导航是在 `mousedown` 阶段决定的，
 * 等到 `mouseup` / `auxclick` 再拦已经晚了。
 */

/** 侧键动作；不是侧键返回 `null`。 */
export type SideButtonAction = "back" | "forward";

/**
 * `MouseEvent.button` → 动作。
 *
 * 3 与 4 是 DOM 规定的「第四键 / 第五键」，在常见鼠标上就是拇指侧的
 * 后退 / 前进两颗。其余按键（含某些鼠标的第六键）一律不认——把不认识的
 * 键也接上去，只会让人按到一颗自己没想按的键时莫名其妙地跳走。
 */
export function sideButtonAction(button: number): SideButtonAction | null {
  if (button === 3) return "back";
  if (button === 4) return "forward";
  return null;
}
