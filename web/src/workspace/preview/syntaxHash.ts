import type { Grammar } from "./highlight";
export const grammar: Grammar = {
  pattern:
    /(#[^\n]*)|("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')|(\b\d+(?:\.\d+)?\b)|\b[A-Za-z_][\w]*\b/g,
  keywords: new Set(
    "and as assert async await break case class continue def del do done elif else esac except export False false fi finally for from function if import in is lambda None not or pass raise return then True true try while with yield".split(
      " ",
    ),
  ),
};
