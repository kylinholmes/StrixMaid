// 运行：先 `bun run dev`，再 `node scripts/workspace-check.mjs`（需要本机有 Edge）。
// SHOT=<目录> 时输出两张截图。CI 不跑它——它要真浏览器。
// 工作区 B 期的真浏览器验证（roadmap/12 §6 的可离线子集）。
// 网络层全部 mock：REST 走 route 拦截，终端 WS 走 routeWebSocket 模拟 PTY 回显。
// 用 Edge（项目记忆：Playwright 用 channel "msedge"）。
import { chromium } from "playwright";

const BASE = "http://localhost:5173";
const results = [];
const check = (name, ok, detail = "") => {
  results.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  — ${detail}` : ""}`);
};

/** 收到的 /api/v1/files 请求 path 参数，按序记录——§6 第 5 条要断言它。 */
const filesRequests = [];

function makeEntries(n, prefix = "f") {
  return Array.from({ length: n }, (_, i) => ({
    name: `${prefix}${String(i).padStart(4, "0")}.txt`,
    kind: "file",
    size_bytes: 100 + i,
    mode: 0o644,
    uid: 1000,
    gid: 1000,
    user: "kylin",
    mtime_ts: 1_758_300_000,
  }));
}

const dir = (name) => ({
  name,
  kind: "dir",
  size_bytes: 0,
  mode: 0o755,
  uid: 1000,
  gid: 1000,
  user: "kylin",
  mtime_ts: 1_758_300_000,
});

/** 每个平台一套目录树 mock。 */
function listings(platform, big) {
  if (platform === "windows") {
    return {
      "\\": { entries: [{ ...dir("C:\\") }, { ...dir("D:\\") }], skipped: 0 },
      "C:\\": { entries: [dir("Users"), dir("Windows")], skipped: 0 },
      "C:\\Users": { entries: [dir("kylin")], skipped: 0 },
      "C:\\Users\\kylin": { entries: [dir("Desktop"), ...makeEntries(3)], skipped: 0 },
    };
  }
  return {
    "/home/kylin": {
      entries: [dir("proj"), dir("docs"), dir(".config"), { ...makeEntries(1)[0], name: ".bashrc" }, ...makeEntries(4)],
      skipped: 0,
    },
    "/home/kylin/proj": { entries: makeEntries(big ? 800 : 6, "p"), skipped: 1 },
    "/home/kylin/docs": { entries: [{ ...makeEntries(1)[0], name: "logo.png" }, ...makeEntries(2, "d")], skipped: 0 },
    "/": { entries: [dir("etc"), dir("home")], skipped: 0 },
    "/etc": { entries: [{ ...makeEntries(1)[0], name: "nginx.conf" }], skipped: 0 },
  };
}

/** POST /terminals 收到的 shell 参数（无则 null），按序记录。 */
const postedShells = [];

/** 终端 WS 收到的 cd 命令行（反向联动的断言点）。 */
const cwdCommands = [];

/** /api/v1/files 收到的完整查询参数（排序/分页的断言点）。 */
const filesQueries = [];

async function mockApi(page, { platform, osId }) {
  const maps = listings(platform, true);
  let terminalSeq = 0;
  const liveTerminals = new Map();

  await page.route("**/api/v1/**", async (route) => {
    const url = new URL(route.request().url());
    const p = url.pathname;
    const method = route.request().method();
    const json = (body, status = 200) =>
      route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });

    if (p.endsWith("/capabilities")) {
      return json({
        system: { systemd: true, journal: true, helper: true, polkit: true, user_units: true, podman: false },
        identity: { hostname: "mock", os_id: osId, os_name: "Mock OS", kernel: "0.0" },
      });
    }
    if (p.endsWith("/auth/session")) {
      return json({
        node: "local", uid: 1000, username: "kylin", groups: ["staff"],
        elevated: false, session_opened: true, authed_ts: 0, created_ts: 0, last_active_ts: 0,
      });
    }
    if (p.endsWith("/system/info")) {
      return json({
        filesystems: [
          { mount_point: platform === "windows" ? "C:\\" : "/", device: "sda1", fs_type: "ext4",
            total_bytes: 1000, used_bytes: 400, available_bytes: 600, read_only: false },
          { mount_point: platform === "windows" ? "D:\\" : "/mnt/nas", device: "nas", fs_type: "nfs",
            total_bytes: 2000, used_bytes: 1000, available_bytes: 1000, read_only: false },
        ],
      });
    }
    if (p.endsWith("/files")) {
      const qs = url.searchParams;
      const qpath = qs.get("path");
      filesRequests.push(qpath);
      filesQueries.push(Object.fromEntries(qs.entries()));
      const found = maps[qpath];
      if (!found) return json({ code: "not_found", message: `没有 ${qpath}` }, 404);
      const key = qs.get("sort") ?? "name";
      const desc = qs.get("order") === "desc";
      const cmp =
        key === "size"
          ? (a, b) => a.size_bytes - b.size_bytes || a.name.localeCompare(b.name)
          : key === "mtime"
            ? (a, b) => a.mtime_ts - b.mtime_ts || a.name.localeCompare(b.name)
            : (a, b) => a.name.localeCompare(b.name);
      const list = [...found.entries].sort(
        (a, b) => (b.kind === "dir") - (a.kind === "dir") || (desc ? cmp(b, a) : cmp(a, b)),
      );
      const total = list.length;
      const offset = Number(qs.get("offset") ?? 0);
      const limit = qs.get("limit") ? Number(qs.get("limit")) : undefined;
      const page = limit === undefined ? list : list.slice(offset, offset + limit);
      return json({ path: qpath, entries: page, skipped: found.skipped, total });
    }
    if (p.endsWith("/files/raw")) {
      // 1×1 红色 PNG。
      const png = Buffer.from(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
        "base64",
      );
      return route.fulfill({ status: 200, contentType: "image/png", body: png });
    }
    if (p.endsWith("/terminals/shells")) {
      return json([
        { path: "/bin/zsh", name: "zsh", default: true },
        { path: "/bin/bash", name: "bash", default: false },
        { path: "/usr/local/bin/pwsh", name: "pwsh", default: false },
      ]);
    }
    if (/\/processes\/\d+$/.test(p)) {
      return json({ cwd: "/home/kylin" });
    }
    if (p.endsWith("/terminals") && method === "POST") {
      const body = route.request().postDataJSON() ?? {};
      postedShells.push(body.shell ?? null);
      const id = `term${++terminalSeq}`;
      liveTerminals.set(id, { shell: body.shell ?? "/bin/zsh" });
      return json({ id }, 201);
    }
    if (p.endsWith("/terminals") && method === "GET") {
      return json(
        [...liveTerminals.entries()].map(([id, t]) => ({
          id, shell: t.shell, user: "kylin", uid: 1000, pid: 4242,
          cols: 80, rows: 24, created_ts: 0, last_active_ts: 0, attached: false,
        })),
      );
    }
    if (/\/terminals\/[^/]+$/.test(p) && method === "DELETE") {
      liveTerminals.delete(p.split("/").pop());
      return route.fulfill({ status: 204, body: "" });
    }
    if (p.endsWith("/metrics/current") || p.endsWith("/system/health")) return json({});
    return json({ code: "not_found", message: p }, 404);
  });

  // 终端 WS：回显打字；整行收齐后当命令处理——`cd` 更新 cwd 并发 OSC 7
  // （pwsh 除外：演「没有 OSC 7」的退化路径），`exit 42` 发 exit 帧并关闭。
  await page.routeWebSocket(/\/ws\/terminal\//, (ws) => {
    const tid = new URL(ws.url()).pathname.split("/").pop();
    const shell = liveTerminals.get(tid)?.shell ?? "/bin/zsh";
    const emitsOsc7 = !shell.includes("pwsh");
    let cwd = "/home/kylin";
    const osc7 = () => {
      if (emitsOsc7) ws.send(Buffer.from(`\x1b]7;file://mock${cwd}\x07`));
    };
    let lineBuf = "";
    ws.onMessage((msg) => {
      if (typeof msg === "string") return; // resize 帧
      const text = Buffer.from(msg).toString();
      ws.send(Buffer.from(text.replace(/\r/g, "\r\n"))); // 极简回显
      lineBuf += text;
      let nl = lineBuf.search(/[\r\n]/);
      while (nl >= 0) {
        const line = lineBuf.slice(0, nl).trim();
        lineBuf = lineBuf.slice(nl + 1);
        nl = lineBuf.search(/[\r\n]/);
        if (line === "exit 42") {
          ws.send(JSON.stringify({ t: "exit", reason: "exited", code: 42 }));
          ws.close();
          return;
        }
        if (line.startsWith("cd ")) {
          cwdCommands.push({ tid, line });
          cwd = line.slice(3).replace(/^['"]|['"]$/g, "");
          osc7();
        }
      }
    });
    // 附着即回放提示符 + 初始 OSC 7（真 bash 的第一个提示符就带）。
    ws.send(Buffer.from("mock-shell $ "));
    osc7();
  });
  await page.routeWebSocket(/\/ws$/, () => {});
}

async function unixFlow(browser) {
  const page = await browser.newPage({ reducedMotion: "reduce" });
  page.on("pageerror", (e) => check("页面无未捕获异常", false, String(e)));
  await mockApi(page, { platform: "unix", osId: "ubuntu" });
  await page.addInitScript(() => localStorage.setItem("strixmaid.session.token", "mock-token"));

  // 从「文件」入口进：面板应收起。
  await page.goto(`${BASE}/files`);
  await page.waitForSelector("text=名称");
  check("文件列表渲染（主目录）", await page.isVisible("text=proj"));
  check("从文件入口进入时终端面板收起", !(await page.isVisible("text=还没有终端")));

  // §4.1：两个导航入口指向同一页，且**每次点击**都重新生效——
  // React Router 复用组件实例，只看首挂载的话第二次点击就没反应了。
  await page.getByRole("button", { name: "终端", exact: true }).click();
  await page.waitForSelector("text=还没有终端");
  check("点导航「终端」展开面板", true);
  await page.getByRole("button", { name: "文件", exact: true }).click();
  await page.waitForTimeout(200);
  check("点导航「文件」收起面板", !(await page.isVisible("text=还没有终端")));

  // ⌃` 切换终端面板（VSCode 同款）。
  await page.keyboard.press("Control+Backquote");
  await page.waitForSelector("text=还没有终端");
  check("Ctrl+` 展开终端面板", true);
  await page.keyboard.press("Control+Backquote");
  await page.waitForTimeout(150);
  check("Ctrl+` 再按折叠面板", !(await page.isVisible("text=还没有终端")));

  // 隐藏文件开关：默认显示 dotfile，开关后隐藏并注明数量。
  check("默认显示隐藏文件", await page.isVisible("text=.bashrc"));
  await page.click('[aria-label="隐藏隐藏文件"]');
  await page.waitForSelector("text=2 个隐藏条目未显示");
  check("开关后 dotfile 不再显示", !(await page.isVisible("text=.bashrc")));
  await page.click('[aria-label="显示隐藏文件"]');
  await page.waitForSelector("text=.bashrc");
  check("再开回来 dotfile 恢复显示", true);

  // 地址栏输入：敲路径回车即跳转。
  await page.fill('[aria-label="路径，回车跳转"]', "/etc");
  await page.press('[aria-label="路径，回车跳转"]', "Enter");
  await page.waitForSelector("text=nginx.conf");
  check("地址栏输入回车跳转", true);

  // 地址栏补全：敲前缀出下拉，↓ 选中 Enter 跳转。
  await page.fill('[aria-label="路径，回车跳转"]', "/home/kylin/pr");
  await page.waitForSelector('[role="option"]:has-text("/home/kylin/proj")', { timeout: 4000 });
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Enter");
  await page.waitForSelector("text=共 800 项");
  check(
    "地址栏补全可选中并跳转",
    (await page.inputValue('[aria-label="路径，回车跳转"]')) === "/home/kylin/proj",
  );
  await page.click("text=主目录");
  await page.waitForSelector('tbody >> text=proj');

  // 进大目录：服务端分页（首页 500）+ 虚拟滚动（DOM 里只有视口附近的行）。
  await page.click("text=proj");
  await page.waitForSelector("text=共 800 项，已加载 500");
  const rows = await page.locator("tbody tr").count();
  check("虚拟滚动只渲染视口附近的行", rows < 120, `DOM 行数 ${rows}`);
  check("skipped 提示", await page.isVisible("text=1 个条目因无权限或已消失被跳过"));
  // 滚到已加载末尾触发下一页，直到 800 全部加载。
  const bigScroller = page.locator('[class*=fileScroll]').first();
  await bigScroller.evaluate((el) => el.scrollTo({ top: el.scrollHeight }));
  await page.waitForFunction(
    () => document.body.textContent?.includes("共 800 项") && !document.body.textContent?.includes("已加载"),
    undefined,
    { timeout: 8000 },
  );
  check("滚动到底自动加载完 800 项", true);
  // 表头点「大小」→ 服务端排序参数带出去。
  await page.click('th button:has-text("大小")');
  await page.waitForTimeout(300);
  check(
    "点表头触发服务端排序",
    filesQueries.some((q) => q.sort === "size" && q.path === "/home/kylin/proj"),
    JSON.stringify(filesQueries.at(-1)),
  );
  await page.click('th button:has-text("名称")');
  await page.waitForTimeout(300);

  // 后退 → 主目录；前进 → 又回来。
  await page.click('[aria-label="后退"]');
  await page.waitForSelector("text=docs");
  check("后退回主目录", true);
  await page.click('[aria-label="前进"]');
  await page.waitForSelector("text=p0000.txt");
  check("前进回大目录", true);

  // 焦点刷新（§4.8）：改 mock 数据 → 触发 visibilitychange → 新条目出现。
  await page.click('[aria-label="后退"]');
  await page.waitForSelector("text=docs");
  await page.route("**/api/v1/files?*", async (route) => {
    const q = new URL(route.request().url()).searchParams.get("path");
    // 只演「主目录里多了个 t1」这一幕，别的目录交回原 mock。
    if (q !== "/home/kylin") return route.fallback();
    filesRequests.push(q);
    return route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        path: q,
        entries: [dir("proj"), dir("docs"), dir("t1"), ...makeEntries(4)],
        skipped: 0,
      }),
    });
  });
  await page.evaluate(() => {
    window.dispatchEvent(new Event("focus"));
  });
  await page.waitForSelector("text=t1", { timeout: 5000 });
  check("焦点刷新后新目录出现", true);

  // 终端：第一个标签用 shell 下拉建（非默认的 bash），验证下拉与参数直达后端。
  await page.click('[aria-label="展开终端面板"]');
  await page.click('[aria-label="选择 shell 新建终端"]');
  await page.getByRole("button", { name: "zsh（默认）" }).waitFor();
  await page.getByRole("button", { name: "bash", exact: true }).click();
  await page.waitForSelector(".xterm", { timeout: 8000 });
  check("shell 下拉建出终端（xterm 挂载）", true);
  check("POST 带上了选中的 shell", postedShells.includes("/bin/bash"), postedShells.join(","));
  check("标签名显示所选 shell", await page.isVisible("text=bash"));
  await page.waitForSelector("text=mock-shell", { timeout: 5000 }).catch(() => {});
  await page.click(".xterm");
  await page.keyboard.type("echo hi");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(300);
  const echoed = await page.evaluate(() => document.querySelector(".xterm")?.textContent ?? "");
  check("键入得到回显", echoed.includes("echo hi"), echoed.slice(0, 80));

  // ---- 目录同步已按负责人决定移除：终端 cd 不影响文件区、导航不注入 cd ----
  await page.click(".xterm");
  await page.keyboard.type("cd /home/kylin/proj");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(600);
  check(
    "终端 cd 不再牵动文件区",
    (await page.inputValue('[aria-label="路径，回车跳转"]')) === "/home/kylin",
  );
  await page.click('tbody >> text=docs');
  await page.waitForSelector("text=d0000.txt");
  await page.waitForTimeout(300);
  check(
    "文件区导航不再向终端注入 cd",
    !cwdCommands.some((c) => c.line.includes("docs")),
    cwdCommands.map((c) => c.line).join(" | "),
  );

  // 平铺视图 + 缩略图：logo.png 出图，目录出图标。
  await page.getByRole("button", { name: "平铺", exact: true }).click();
  await page.waitForSelector('[class*=tileGrid]');
  await page.waitForSelector('[class*=tileIcon] img[src^="blob:"]', { timeout: 5000 });
  check("平铺视图的图片条目出缩略图", true);
  await page.getByRole("button", { name: "列表", exact: true }).click();
  await page.waitForSelector("text=d0000.txt");

  // 第二个标签 + 切换。
  await page.click('[aria-label="新建终端"]');
  await page.waitForFunction(() => document.querySelectorAll(".xterm").length === 2);
  check("第二个标签，两个 xterm 实例并存（切走不卸载）", true);

  // 在第二个标签里 exit 42。
  await page.click(".xterm >> nth=1");
  await page.keyboard.type("exit 42");
  await page.keyboard.press("Enter");
  await page.waitForSelector("text=已退出 (code 42)", { timeout: 5000 });
  check("退出显示带退出码，且与断线区分", true);
  check("退出后内容保留（xterm 未销毁）",
    (await page.locator(".xterm").count()) === 2);

  if (process.env.SHOT) {
    await page.screenshot({ path: `${process.env.SHOT}/workspace-terminal.png` });
  }

  // 已退出的标签不占名额（服务端已释放）：8 个里有 1 个已退出时 + 仍可用。
  while ((await page.locator(".xterm").count()) < 8) {
    await page.click('[aria-label="新建终端"]');
    await page.waitForTimeout(120);
  }
  check(
    "8 个标签但 1 个已退出时 + 仍可用",
    !(await page.locator('[aria-label="新建终端"]').isDisabled()),
  );
  // 开满 8 个活的：`+` 必须禁用（§7-2），UI 自己算、后端从未回 409。
  await page.click('[aria-label="新建终端"]');
  await page.waitForFunction(() => document.querySelectorAll(".xterm").length === 9);
  check("到 8 个存活上限时 + 禁用", await page.locator('[aria-label="新建终端"]').isDisabled());

  // 刷新保住滚动位置（§7-3）：进大目录、滚下去、焦点刷新、位置不动。
  await page.click('[aria-label="折叠终端面板"]');
  await page.click("text=主目录");
  await page.waitForSelector('tbody >> text=proj');
  await page.click('tbody >> text=proj');
  await page.waitForSelector("text=共 800 项");
  const scroller = page.locator(".fileScroll, [class*=fileScroll]").first();
  await scroller.evaluate((el) => el.scrollTo({ top: 1500 }));
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
  await page.waitForTimeout(500);
  const top = await scroller.evaluate((el) => el.scrollTop);
  check("焦点刷新后滚动位置未跳顶", top === 1500, `scrollTop=${top}`);

  if (process.env.SHOT) {
    await page.click('[aria-label="后退"]');
    await page.waitForSelector("text=t1");
    await page.screenshot({ path: `${process.env.SHOT}/workspace-files.png` });
  }

  await page.close();
}

async function windowsFlow(browser) {
  const page = await browser.newPage({ reducedMotion: "reduce" });
  await mockApi(page, { platform: "windows", osId: "windows" });
  await page.addInitScript(() => localStorage.setItem("strixmaid.session.token", "mock-token"));
  filesRequests.length = 0;

  await page.goto(`${BASE}/files`);
  // 主目录猜成 C:\Users\kylin。
  await page.waitForSelector("text=Desktop");
  check("Windows 主目录启发式生效", filesRequests.includes("C:\\Users\\kylin"));

  // 全部驱动器 → 点 C:\ → 请求必须是 C:\，绝不能是 \C:\（§6 第 5 条）。
  await page.click("text=全部驱动器");
  await page.waitForSelector("tbody >> text=C:\\");
  await page.click("tbody >> text=C:\\");
  await page.waitForSelector("text=Windows");
  const bad = filesRequests.filter((q) => q?.includes("\\C:"));
  check("驱动器根不与父路径拼接", bad.length === 0, bad.join(","));
  check("确实请求了 C:\\", filesRequests.includes("C:\\"));

  // 上一级：C:\ → 虚拟根 \。
  await page.click('[aria-label="上一级"]');
  await page.waitForSelector("tbody >> text=D:\\");
  check("盘根的上一级是全部驱动器", filesRequests.includes("\\"));
  await page.close();
}

const browser = await chromium.launch({ channel: "msedge", headless: true });
try {
  await unixFlow(browser);
  await windowsFlow(browser);
} finally {
  await browser.close();
}

const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} 项通过`);
process.exit(failed.length === 0 ? 0 : 1);
