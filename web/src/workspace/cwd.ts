/**
 * cwd 双向联动的纯函数部分（`docs/roadmap/12-workspace.md` §4.4）。
 *
 * 联动**只走 OSC 7**（shell 自己报，`ESC ] 7 ; file://host/path BEL`）。
 * 曾设计过按 `TerminalInfo.pid` 轮询进程 cwd 的兜底，砍掉了：2s 滞后的在途
 * 旧值会把文件区来回拽，PowerShell 更是静默给错值（`Set-Location` 不改进程
 * cwd，实测 §2.3）——宁可明确不跟随并给出启用片段，不可指错。
 */

/**
 * 解析 OSC 7 的数据部分（xterm 交来的是 `7;` 之后、终止符之前的载荷，
 * 即 `file://host/path`）。返回解码后的绝对路径；不认识的形状返回 `null`
 * ——错的来源地址会把文件区引到别处去，宁可不动。
 */
export function parseOsc7(payload: string): string | null {
  if (!payload.startsWith("file://")) return null;
  const rest = payload.slice("file://".length);
  // host 段到第一个 `/` 为止（可为空：`file:///path`）。host 是谁发的就是谁，
  // 不做校验——终端连的本来就是那台机器。
  const slash = rest.indexOf("/");
  if (slash < 0) return null;
  let path = rest.slice(slash);
  try {
    path = decodeURIComponent(path);
  } catch {
    // 不是合法的百分号编码就按原样用（bash 注入的就是未编码的 $PWD）。
  }
  // Windows 侧的约定形状 `file://host/C:/x`：去掉引导斜杠并翻转分隔符。
  if (/^\/[A-Za-z]:\//.test(path) || /^\/[A-Za-z]:$/.test(path)) {
    return path.slice(1).replaceAll("/", "\\");
  }
  return path;
}

/** shell 路径的文件名（小写）。 */
function shellName(shell: string | undefined): string {
  if (!shell) return "";
  const cut = Math.max(shell.lastIndexOf("/"), shell.lastIndexOf("\\"));
  return (cut >= 0 ? shell.slice(cut + 1) : shell).toLowerCase();
}

/** PowerShell（5.1 或 7+）。 */
export function isPowerShell(shell: string | undefined): boolean {
  const n = shellName(shell);
  return n === "powershell.exe" || n === "pwsh.exe" || n === "pwsh";
}

/** zsh（说明条要给它专属的 chpwd 片段）。 */
export function isZsh(shell: string | undefined): boolean {
  return shellName(shell) === "zsh";
}

/**
 * 反向联动：把「到这个目录去」组装成发给 shell 的一行命令字节。
 *
 * 按 shell 方言引用，路径里的引号也要活下来：
 * - POSIX：单引号 + `'\''` 逃逸；
 * - PowerShell：单引号 + `''` 逃逸；
 * - cmd.exe：双引号 + `/d`（跨盘也能切）。
 */
export function buildCdBytes(path: string, shell: string | undefined): Uint8Array {
  const n = shellName(shell);
  let line: string;
  if (n === "cmd.exe") {
    line = `cd /d "${path}"\r\n`;
  } else if (isPowerShell(shell)) {
    line = `cd '${path.replaceAll("'", "''")}'\r\n`;
  } else {
    line = `cd '${path.replaceAll("'", "'\\''")}'\n`;
  }
  return new TextEncoder().encode(line);
}

/** 给 zsh 用户的 `~/.zshrc` 片段：chpwd 钩子 + 启动时报一次。 */
export const ZSH_OSC7_SNIPPET =
  '_osc7(){ printf \'\\e]7;file://%s%s\\a\' "$HOST" "$PWD"; }; autoload -Uz add-zsh-hook; add-zsh-hook chpwd _osc7; _osc7';

/** 给 PowerShell 用户的 `$PROFILE` 片段：加上它就有 OSC 7，联动即启用。 */
export const POWERSHELL_OSC7_SNIPPET =
  "function prompt { $e=[char]27; $b=[char]7; Write-Host -NoNewline \"$e]7;file://$env:COMPUTERNAME$($pwd.Path -replace '\\\\','/')$b\"; \"PS $pwd> \" }";
