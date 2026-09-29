import type { Grammar } from "./highlight";
export const grammar: Grammar = {
  pattern:
    /(\/\/[^\n]*|\/\*[\s\S]*?(?:\*\/|$))|("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`)|(\b\d+(?:\.\d+)?\b)|\b[A-Za-z_$][\w$]*\b/g,
  keywords: new Set(
    "async await break case catch class const continue default do else enum export extends false finally fn for from function if impl import in interface let match mod new null package private pub public return static struct super switch this throw trait true try type typeof undefined use var void while yield".split(
      " ",
    ),
  ),
};
