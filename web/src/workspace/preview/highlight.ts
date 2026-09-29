import { extension } from "./kind";
export const MAX_HIGHLIGHT_TOKENS = 20_000;
export interface Token {
  text: string;
  kind?: "keyword" | "string" | "comment" | "number";
  offset: number;
}
export interface Grammar {
  pattern: RegExp;
  keywords: ReadonlySet<string>;
}
export async function highlight(name: string, text: string): Promise<Token[] | null> {
  const ext = extension(name);
  const grammar = /^(js|jsx|ts|tsx|mjs|cjs|json|jsonc|rs|c|h|cpp|hpp|java|go)$/.test(ext)
    ? (await import("./syntaxC")).grammar
    : /^(py|sh|bash|zsh|yaml|yml|toml)$/.test(ext)
      ? (await import("./syntaxHash")).grammar
      : null;
  if (!grammar) return null;
  const tokens: Token[] = [];
  let offset = 0;
  // 每次使用独立 RegExp，跨预览没有 lastIndex 状态；只做词法着色，不解释标记。
  for (const match of text.matchAll(new RegExp(grammar.pattern))) {
    if (match.index > offset) tokens.push({ text: text.slice(offset, match.index), offset });
    const value = match[0];
    const kind = match[1]
      ? "comment"
      : match[2]
        ? "string"
        : match[3]
          ? "number"
          : grammar.keywords.has(value)
            ? "keyword"
            : undefined;
    tokens.push({ text: value, kind, offset: match.index });
    offset = match.index + value.length;
    // 密集 token 的文本无需创建几十万个 React 节点；可靠退回完整纯文本。
    if (tokens.length > MAX_HIGHLIGHT_TOKENS) return null;
  }
  if (offset < text.length) tokens.push({ text: text.slice(offset), offset });
  return tokens.length > MAX_HIGHLIGHT_TOKENS ? null : tokens;
}
