/**
 * class 组合器。
 *
 * CSS Modules 的导出是索引签名，在 `noUncheckedIndexedAccess` 下每个 `s.foo`
 * 都是 `string | undefined`。这个组合是有价值的（数组与 Record 的越界能被类型抓住），
 * 所以不关它，而是让所有 class 拼接都过这个函数。
 */
export function cx(...parts: (string | false | null | undefined)[]): string {
  return parts.filter(Boolean).join(" ");
}
