import { expect, type Page, type Route, test } from "playwright/test";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
const entry = (cursor: string) => ({ cursor, message: cursor, priority: "warning", ts: 1, us: 0 });
const readState = (page: Page) =>
  page
    .getByTestId("state")
    .textContent()
    .then((s) => JSON.parse(s!));

async function logsSocket(page: Page) {
  const ready = deferred<(rows: ReturnType<typeof entry>[]) => void>();
  await page.routeWebSocket("**/ws", (ws) => {
    ws.onMessage((raw) => {
      const msg = JSON.parse(String(raw));
      if (msg.t === "sub") {
        ws.send(JSON.stringify({ v: 1, t: "resp", ch: msg.ch }));
        ready.resolve((rows) =>
          ws.send(JSON.stringify({ v: 1, t: "data", ch: "logs.follow", d: rows })),
        );
      }
    });
  });
  return ready;
}

test("StrictMode 翻页只发一次；换筛选取消旧页，重复游标也不串数据", async ({ page }) => {
  await logsSocket(page);
  const delayed = deferred<Route>();
  let oldPages = 0;
  await page.route("**/api/v1/logs?*", async (route) => {
    const q = new URL(route.request().url()).searchParams;
    if (q.get("unit") === "a" && q.has("cursor")) {
      oldPages++;
      delayed.resolve(route);
      return;
    }
    const prefix = q.get("unit")!;
    await route.fulfill({
      json: {
        entries: [entry(`${prefix}-${q.has("cursor") ? "older" : "head"}`)],
        next_cursor: q.has("cursor") ? null : "same-cursor",
      },
    });
  });
  await page.goto("/e2e/logs.html");
  await expect(page.getByTestId("messages")).toHaveText("a-head");
  await page.getByRole("button", { name: "翻页", exact: true }).click();
  const old = await delayed.promise;
  await page.getByRole("button", { name: "换筛选" }).click();
  await expect(page.getByTestId("messages")).toHaveText("b-head");
  await expect.poll(() => readState(page).then((s) => s.busy)).toBe(false);
  await old
    .fulfill({ json: { entries: [entry("private-old")], next_cursor: "stale" } })
    .catch(() => {});
  await page.getByRole("button", { name: "翻页", exact: true }).click();
  await expect(page.getByTestId("messages")).toHaveText("b-head\nb-older");
  expect(oldPages).toBe(1);
  expect((await readState(page)).hasMore).toBe(false);
});

test("暂停跟随超过容量，回到最新后从连续记录末尾恢复分页", async ({ page }) => {
  const socket = await logsSocket(page);
  const cursors: string[] = [];
  await page.route("**/api/v1/logs?*", async (route) => {
    const cursor = new URL(route.request().url()).searchParams.get("cursor");
    if (cursor) cursors.push(cursor);
    await route.fulfill({
      json: {
        entries: [entry(cursor ? "999" : "old-page")],
        next_cursor: cursor ? null : "old-cursor",
      },
    });
  });
  await page.goto("/e2e/logs.html");
  await expect(page.getByTestId("messages")).toHaveText("old-page");
  await page.getByRole("button", { name: "暂停跟随" }).click();
  const send = await socket.promise;
  for (let start = 0; start < 6_000; start += 200) {
    send(Array.from({ length: 200 }, (_, i) => entry(String(start + i))));
  }
  await expect.poll(() => readState(page).then((s) => s.pending)).toBe(5_000);
  await expect.poll(() => readState(page).then((s) => s.truncated)).toBe(true);
  await page.getByRole("button", { name: "回到最新" }).click();
  await expect
    .poll(() => readState(page).then((s) => [s.first, s.last, s.count, s.pending]))
    .toEqual(["5999", "1000", 5_000, 0]);
  await page.getByRole("button", { name: "翻页", exact: true }).click();
  await expect.poll(() => readState(page).then((s) => s.last)).toBe("999");
  expect(cursors).toEqual(["1000"]);
  expect((await readState(page)).count).toBe(5_000);
});

test("真实登录界面换用户：注销挂起仍立即锁定，目录缓存和私有挂载点不复用", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.routeWebSocket("**/ws", (ws) => {
    ws.onMessage(() => {});
  });
  const logout = deferred<Route>();
  const paths: string[] = [];
  await page.route("**/api/v1/**", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const username = request.headers().authorization === "Bearer bob-token" ? "bob" : "alice";
    const json = (body: unknown) => route.fulfill({ json: body });
    switch (url.pathname) {
      case "/api/v1/capabilities":
        return json({
          identity: { os_id: "linux", hostname: "test", os_name: "Test Linux" },
          system: { helper: true, procfs: true, journal: true },
        });
      case "/api/v1/auth/start":
        return json({
          session: request.postDataJSON().username,
          prompts: [{ id: 1, style: "prompt", text: "Password:" }],
        });
      case "/api/v1/auth/respond": {
        const name = request.postDataJSON().session;
        return json({
          status: "complete",
          token: `${name}-token`,
          user: { username: name, uid: 1000, groups: [] },
        });
      }
      case "/api/v1/auth/logout":
        logout.resolve(route);
        return;
      case "/api/v1/system/info":
        return json({
          filesystems:
            username === "alice" ? [{ mount_point: "/mnt/alice-private", fs_type: "ext4" }] : [],
        });
      case "/api/v1/files": {
        const path = url.searchParams.get("path")!;
        paths.push(`${username}:${path}`);
        return json({
          path,
          total: 1,
          entries: [
            {
              name: `${username}-secret.txt`,
              kind: "file",
              mode: 420,
              uid: 1000,
              gid: 1000,
              size_bytes: 5,
              mtime_ts: 1,
            },
          ],
        });
      }
      case "/api/v1/terminals":
        return json([]);
      default:
        return route.fulfill({
          status: 503,
          json: { code: "unavailable", message: "测试不提供此能力" },
        });
    }
  });
  await page.goto("/files");
  const login = async (name: string) => {
    await page.getByRole("textbox", { name: "用户名", exact: true }).fill(name);
    await page.getByLabel("密码", { exact: true }).fill("test-only");
    await page.getByRole("button", { name: "登录", exact: true }).click();
  };
  await login("alice");
  await expect(page.getByText("alice-secret.txt", { exact: true }).first()).toBeVisible();
  await expect(page.getByText("alice-private", { exact: true })).toBeVisible();
  await page.getByRole("combobox", { name: "路径，回车跳转", exact: true }).fill("/shared");
  await page.getByRole("combobox", { name: "路径，回车跳转", exact: true }).press("Enter");
  await expect.poll(() => paths.includes("alice:/shared")).toBe(true);
  await page.getByRole("button", { name: "锁定", exact: true }).click();
  const oldLogout = await logout.promise;
  await expect(page.getByRole("heading", { name: "登录", exact: true })).toBeVisible();
  await expect(page.getByText("alice-secret.txt", { exact: true })).toHaveCount(0);
  await login("bob");
  await expect(page.getByText("bob-secret.txt", { exact: true }).first()).toBeVisible();
  await expect(page.getByText("alice-private", { exact: true })).toHaveCount(0);
  await oldLogout.fulfill({ status: 204 });
  await expect(page.getByText("bob-secret.txt", { exact: true }).first()).toBeVisible();
  expect(paths).toContain("bob:/home/bob");
  expect(paths).not.toContain("bob:/home/alice");
  expect(paths).not.toContain("bob:/shared");
  await expect(page.getByRole("combobox", { name: "路径，回车跳转", exact: true })).toHaveValue(
    "/home/bob",
  );
  await page.getByRole("combobox", { name: "路径，回车跳转", exact: true }).fill("/shared");
  await page.getByRole("combobox", { name: "路径，回车跳转", exact: true }).press("Enter");
  await expect.poll(() => paths.includes("bob:/shared")).toBe(true);
  await expect(page.getByText("bob-secret.txt", { exact: true }).first()).toBeVisible();
  await expect(page.getByText("alice-secret.txt", { exact: true })).toHaveCount(0);
  expect(errors).toEqual([]);
});

test("迟到的 REST 首页保留等待期间的实时日志，并去掉重复游标", async ({ page }) => {
  const socket = await logsSocket(page);
  const pending: Route[] = [];
  await page.route("**/api/v1/logs?*", (route) => {
    pending.push(route);
  });
  await page.goto("/e2e/logs.html");
  const send = await socket.promise;
  send([entry("overlap"), entry("live")]);
  await expect(page.getByTestId("messages")).toHaveText("live\noverlap");
  // StrictMode 首次挂载会取消上一轮：首页的所有请求都可完成，只有当前一轮能写状态。
  for (const route of pending) {
    await route
      .fulfill({ json: { entries: [entry("overlap"), entry("history")], next_cursor: null } })
      .catch(() => {});
  }
  await expect.poll(() => readState(page).then((s) => s.loaded)).toBe(true);
  await expect(page.getByTestId("messages")).toHaveText("live\noverlap\nhistory");
});
