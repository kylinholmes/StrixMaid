import { readFile } from "node:fs/promises";
import { expect, type Page, test } from "playwright/test";
import { closePreviewServers, previewServer, serveBytes } from "./preview-server";

test.afterEach(() => closePreviewServers());

const entry = (name: string, size_bytes = 20, kind = "file") => ({
  name,
  size_bytes,
  kind,
  mode: 420,
  uid: 1000,
  gid: 1000,
  mtime_ts: 1,
});
const files = [
  entry("code.ts"),
  entry("unsafe.html"),
  entry("image.svg"),
  entry("large.txt", 655361),
  entry("huge.bin", 5 * 1024 ** 3),
  entry("photo.jpg", 24 * 1024 ** 2),
  entry("picture.heic"),
  entry("paper.pdf"),
  entry("sound.wav"),
  entry("movie.webm"),
  entry("pipe", 0, "fifo"),
  entry("folder", 0, "dir"),
  { ...entry("link", 0, "symlink"), target: "/elsewhere/target.txt", target_kind: "file" },
];
const unsafe =
  '<script>window.previewExecuted=true</script><svg onload="window.previewExecuted=true"></svg>';
interface Call {
  method: string;
  path: string;
  body?: { path: string; purpose: string };
  auth?: string;
  range?: string;
  cookie?: string;
  resourceType?: string;
}
async function setup(page: Page, origin = "", nativeContent = false) {
  const calls: Call[] = [];
  const records = new Map<string, { path: string; purpose: string }>();
  let count = 0;
  await page.route("**/api/v1/**", async (route) => {
    const req = route.request();
    const url = new URL(req.url());
    const call: Call = {
      method: req.method(),
      path: url.pathname,
      auth: req.headers().authorization,
      range: req.headers().range,
      cookie: req.headers().cookie,
      resourceType: req.resourceType(),
    };
    calls.push(call);
    if (url.pathname === "/api/v1/files")
      return route.fulfill({
        json: { path: url.searchParams.get("path"), entries: files, total: files.length },
      });
    if (url.pathname === "/api/v1/files/content") {
      const path = url.searchParams.get("path")!;
      return route.fulfill({
        json: {
          path,
          content: /html|svg/.test(path)
            ? unsafe
            : 'const message = "你好，StrixMaid 👩🏽‍💻";\n\n// 预览保持原文：HTML 只作为字符串显示\nexport async function readPreview(path: string) {\n  const response = await fetch(path);\n  if (!response.ok) {\n    throw new Error("文件读取失败");\n  }\n  return response.text();\n}\n\nconst html = "<script>alert(1)</script>";\nconst limit = 640 * 1024;\n',
          size_bytes: 30,
          lossy: true,
          truncated: false,
        },
      });
    }
    if (url.pathname === "/api/v1/file-access") {
      call.body = req.postDataJSON();
      const id = `record-${++count}`;
      records.set(id, call.body!);
      return route.fulfill({
        headers: {
          "Set-Cookie": "files=fixture-cookie; Path=/api/v1/file-access; HttpOnly; SameSite=Strict",
        },
        json: { id, url: `/api/v1/file-access/${id}/content`, expires_in_secs: 600 },
      });
    }
    if (req.method() === "DELETE") return route.fulfill({ status: 204 });
    if (url.pathname.endsWith("/renew")) {
      const id = url.pathname.split("/")[4];
      return route.fulfill({
        json: { id, url: `/api/v1/file-access/${id}/content`, expires_in_secs: 600 },
      });
    }
    if (url.pathname.endsWith("/content")) {
      if (nativeContent) return route.continue();
      const record = records.get(url.pathname.split("/")[4]!)!;
      if (record.purpose === "download")
        return route.fulfill({
          contentType: "application/octet-stream",
          headers: { "Content-Disposition": 'attachment; filename="download.bin"' },
          body: "native-download-fixture",
        });
      return route.fulfill({ status: 415, body: "Unsupported fixture" });
    }
    return route.fulfill({ status: 404 });
  });
  await page.goto(`${origin}/e2e/preview.html`);
  await expect(
    page.locator('[data-pane="top"]').getByText("code.ts", { exact: true }),
  ).toBeVisible();
  return { calls, records };
}
function row(page: Page, name: string) {
  return page
    .locator('[data-pane="top"] tbody tr')
    .filter({ has: page.getByText(name, { exact: true }) });
}
async function switchFile(page: Page, name: string) {
  await page.evaluate((name) => {
    window.dispatchEvent(
      new CustomEvent("preview:switch", {
        detail: { path: `/fixtures/${name}`, entry: { name, kind: "file", size_bytes: 20 } },
      }),
    );
  }, name);
}

test("列表单击选中，双击/Enter预览；关闭恢复焦点，代码按需高亮", async ({ page }) => {
  const { calls } = await setup(page);
  await row(page, "code.ts").click();
  await expect(row(page, "code.ts")).toHaveAttribute("aria-selected", "true");
  await expect(page.getByRole("dialog")).toHaveCount(0);
  await row(page, "code.ts").dblclick();
  await expect(page.getByRole("dialog")).toBeVisible();
  await expect(page.locator('code [data-token="keyword"]').first()).toHaveText("const");
  await expect(page.getByText(/无效 UTF-8/)).toBeVisible();
  await page.getByRole("button", { name: "关闭预览" }).focus();
  await page.keyboard.press("Shift+Tab");
  await page.keyboard.press("Tab");
  await expect(page.getByRole("button", { name: "关闭预览" })).toBeFocused();
  await expect(page.getByRole("button", { name: "关闭预览" })).toHaveCSS("outline-style", "solid");
  await page.screenshot({ path: "/tmp/strixmaid-files-preview-text.png" });
  await page.getByRole("button", { name: "关闭预览" }).click();
  await expect(row(page, "code.ts")).toBeFocused();
  await row(page, "code.ts").press("Enter");
  await expect(page.getByRole("dialog")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(row(page, "code.ts")).toBeFocused();
  expect(
    calls
      .filter((c) => c.path === "/api/v1/files/content")
      .every((c) => c.auth === "Bearer preview-test"),
  ).toBe(true);
});

test("平铺保留目录/链接行为，文件单击选择且键盘打开", async ({ page }) => {
  await setup(page);
  await page.getByRole("button", { name: "平铺", exact: true }).click();
  const tile = page.getByRole("button", { name: "code.ts", exact: true });
  await tile.click();
  await expect(page.getByRole("dialog")).toHaveCount(0);
  await tile.press("Enter");
  await expect(page.getByRole("dialog")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(tile).toBeFocused();
  await tile.dblclick();
  await expect(page.getByRole("dialog")).toBeVisible();
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "folder", exact: true }).click();
  await expect(page.getByRole("combobox")).toHaveValue("/fixtures/folder");
  await page.getByRole("button", { name: "link", exact: true }).click();
  await expect(page.getByRole("combobox")).toHaveValue("/elsewhere");
});

test("HTML/SVG只作文本，设备与超限文本不发读取请求", async ({ page }) => {
  const { calls } = await setup(page);
  for (const name of ["unsafe.html", "image.svg"]) {
    await row(page, name).dblclick();
    await expect(page.locator("pre code")).toHaveText(unsafe);
    await expect(page.locator("dialog iframe, dialog svg, dialog script")).toHaveCount(0);
    await page.keyboard.press("Escape");
  }
  expect(await page.evaluate(() => "previewExecuted" in window)).toBe(false);
  const before = calls.filter((c) => /content$|file-access$/.test(c.path)).length;
  await row(page, "large.txt").dblclick();
  await expect(page.getByText(/超过 640 KiB/)).toBeVisible();
  await page.keyboard.press("Escape");
  await row(page, "pipe").dblclick();
  await expect(page.getByText(/不能预览或下载/)).toBeVisible();
  await expect(page.getByRole("button", { name: "下载", exact: true })).toBeDisabled();
  expect(calls.filter((c) => /content$|file-access$/.test(c.path))).toHaveLength(before);
});

test("大文件原生下载只创建download记录，不fetch或Blob整文件", async ({ page }) => {
  const server = await previewServer((_url, _range, response) => {
    response.writeHead(200, {
      "Content-Type": "application/octet-stream",
      "Content-Disposition": 'attachment; filename="download.bin"',
    });
    response.end("native-download-fixture");
  });
  const { calls } = await setup(page, server.url, true);
  await page.evaluate(() => {
    URL.createObjectURL = () => {
      throw new Error("禁止下载Blob");
    };
  });
  await row(page, "huge.bin").dblclick();
  await expect(page.getByText(/此文件类型不支持/)).toBeVisible();
  const pending = page.waitForEvent("download");
  await page.getByRole("button", { name: "下载", exact: true }).click();
  const download = await pending;
  expect(await download.failure()).toBeNull();
  expect(await readFile((await download.path())!, "utf8")).toBe("native-download-fixture");
  expect(calls.find((c) => c.path === "/api/v1/file-access")?.body).toEqual({
    path: "/fixtures/huge.bin",
    purpose: "download",
  });
  await page.keyboard.press("Escape");
  expect(calls.filter((c) => c.method === "DELETE")).toHaveLength(0);
  await page.evaluate(() => window.dispatchEvent(new Event("preview:reset")));
  await expect.poll(() => calls.filter((c) => c.method === "DELETE").length).toBe(1);
  server.close();
});

test("PDF受限sandbox、5min续期、下载独立；sessionreset关闭并释放旧凭据", async ({ page }) => {
  const server = await previewServer((_url, _range, response) => {
    response.writeHead(200, { "Content-Type": "application/pdf" });
    response.end(
      "%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Count 0/Kids[]>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF",
    );
  });
  const { calls } = await setup(page, server.url, true);
  await page.clock.install();
  await row(page, "paper.pdf").dblclick();
  const frame = page.getByTitle("PDF 预览：paper.pdf");
  await expect(frame).toHaveAttribute("sandbox", "");
  await expect(frame).toHaveAttribute("referrerpolicy", "no-referrer");
  const src = await frame.getAttribute("src");
  expect(src).toMatch(/^\/api\/v1\/file-access\/record-\d+\/content$/);
  await page.clock.fastForward(300_000);
  await expect.poll(() => calls.some((c) => c.path.endsWith("/renew"))).toBe(true);
  const pending = page.waitForEvent("download");
  await page.getByRole("button", { name: "下载", exact: true }).click();
  expect(await (await pending).failure()).toBeNull();
  await page.evaluate(() => window.dispatchEvent(new Event("preview:reset")));
  await expect(page.getByRole("dialog")).toHaveCount(0);
  await expect.poll(() => calls.filter((c) => c.method === "DELETE").length).toBe(2);
  expect(
    calls.filter((c) => c.method === "DELETE").every((c) => c.auth === "Bearer preview-test"),
  ).toBe(true);
});

test("切换文本取消旧请求，迟到响应不能覆盖新文件", async ({ page }) => {
  await page.addInitScript(() => {
    const original = window.fetch;
    window.fetch = (input, init) => {
      const request = new Request(input, init);
      if (request.url.includes("files/content") && request.url.includes("slow.txt")) {
        request.signal.addEventListener("abort", () => {
          document.documentElement.dataset.oldAborted = "true";
        });
      }
      return original(input, init);
    };
  });
  await setup(page);
  await page.route("**/files/content?path=*slow.txt", () => {});
  await switchFile(page, "slow.txt");
  await expect(page.getByText("正在读取预览…")).toBeVisible();
  await switchFile(page, "new.ts");
  await expect(page.locator("pre code")).toContainText("const message");
  await expect(page.locator("html")).toHaveAttribute("data-old-aborted", "true");
});

test("图片走preview记录，解码或媒体不支持时可下载", async ({ page }) => {
  const { calls } = await setup(page);
  for (const name of ["photo.jpg", "picture.heic", "sound.wav", "movie.webm"]) {
    await row(page, name).dblclick();
    await expect(page.getByText(/浏览器无法预览/)).toBeVisible();
    await expect(page.getByRole("button", { name: "下载", exact: true })).toBeEnabled();
    await page.keyboard.press("Escape");
  }
  expect(calls.filter((c) => c.path === "/api/v1/file-access").map((c) => c.body?.purpose)).toEqual(
    ["preview", "preview", "preview", "preview"],
  );
  await expect.poll(() => calls.filter((c) => c.method === "DELETE").length).toBe(4);
});

test("1600px图片显示且不二次旋转，关闭释放URL记录", async ({ page }) => {
  const image = await readFile(new URL("./fixtures/preview-image.png", import.meta.url));
  const server = await previewServer((_url, range, response) =>
    serveBytes(response, image, "image/png", range),
  );
  try {
    const { calls } = await setup(page, server.url, true);
    await row(page, "photo.jpg").dblclick();
    const imageView = page.getByRole("img", { name: "photo.jpg", exact: true });
    await expect
      .poll(() => imageView.evaluate((el) => (el as HTMLImageElement).naturalWidth))
      .toBe(1600);
    await expect(imageView).toHaveCSS("transform", "none");
    expect(calls.find((call) => call.path.endsWith("/content"))).toMatchObject({
      resourceType: "image",
      cookie: "files=fixture-cookie",
      auth: undefined,
    });
    expect(await page.evaluate(() => document.cookie)).not.toContain("fixture-cookie");
    await page.getByRole("button", { name: "关闭预览" }).focus();
    await page.keyboard.press("Shift+Tab");
    await page.keyboard.press("Tab");
    await expect(page.getByRole("button", { name: "关闭预览" })).toBeFocused();
    await expect(page.getByRole("button", { name: "关闭预览" })).toHaveCSS(
      "outline-style",
      "solid",
    );
    await page.screenshot({ path: "/tmp/strixmaid-files-preview-image.png" });
    await page.keyboard.press("Escape");
    await expect.poll(() => calls.some((c) => c.method === "DELETE")).toBe(true);
  } finally {
    server.close();
  }
});

function wave(): Buffer {
  const dataSize = 60 * 44100 * 2;
  const bytes = Buffer.alloc(44 + dataSize);
  bytes.write("RIFF");
  bytes.writeUInt32LE(36 + dataSize, 4);
  bytes.write("WAVEfmt ", 8);
  bytes.writeUInt32LE(16, 16);
  bytes.writeUInt16LE(1, 20);
  bytes.writeUInt16LE(1, 22);
  bytes.writeUInt32LE(44100, 24);
  bytes.writeUInt32LE(88200, 28);
  bytes.writeUInt16LE(2, 32);
  bytes.writeUInt16LE(16, 34);
  bytes.write("data", 36);
  bytes.writeUInt32LE(dataSize, 40);
  return bytes;
}
for (const kind of ["audio", "video"] as const) {
  test(`${kind}原生URL与controls，seek触发非零Range`, async ({ page }) => {
    const body =
      kind === "audio"
        ? wave()
        : await readFile(new URL("./fixtures/preview-seek.webm", import.meta.url));
    const ranges: string[] = [];
    const server = await previewServer((_url, range, response) => {
      if (range) ranges.push(range);
      serveBytes(
        response,
        body,
        kind === "audio" ? "audio/wav" : "video/webm",
        range,
        range === "bytes=0-",
      );
    });
    try {
      await setup(page, server.url, true);
      await row(page, kind === "audio" ? "sound.wav" : "movie.webm").dblclick();
      const media = page.locator(`dialog ${kind}`);
      await expect(media).toHaveAttribute("controls", "");
      await expect(media).toHaveAttribute("src", /^\/api\/v1\/file-access\//);
      await expect
        .poll(() => media.evaluate((el) => (el as HTMLMediaElement).readyState))
        .toBeGreaterThanOrEqual(1);
      await media.evaluate((el) => {
        (el as HTMLMediaElement).currentTime = (el as HTMLMediaElement).duration - 2;
      });
      await expect.poll(() => media.evaluate((el) => (el as HTMLMediaElement).seeking)).toBe(false);
      await expect
        .poll(() => media.evaluate((el) => (el as HTMLMediaElement).currentTime))
        .toBeGreaterThan(kind === "audio" ? 50 : 15);
      await expect.poll(() => ranges.some((range) => /^bytes=[1-9]\d*-/.test(range))).toBe(true);
      expect(ranges[0]).toBe("bytes=0-");
      await test.info().attach(`${kind}-ranges.json`, {
        body: JSON.stringify({ bytes: body.length, ranges }),
        contentType: "application/json",
      });
    } finally {
      server.close();
    }
  });
}

test("lazy高亮加载失败仍完整显示Unicode和恶意HTML文本", async ({ page }) => {
  await setup(page);
  await page.route("**/src/workspace/preview/highlight.ts*", (route) => route.abort());
  const source =
    'const 名字 = "你好👩🏽‍💻e\u0301𠮷";\n<script>window.previewExecuted=true</script>';
  await page.route("**/files/content?*", (route) =>
    route.fulfill({
      json: {
        path: "/fixtures/code.ts",
        content: source,
        size_bytes: 100,
        truncated: false,
        lossy: false,
      },
    }),
  );
  await row(page, "code.ts").dblclick();
  await expect(page.locator("pre code")).toHaveText(source);
  await expect(page.locator("pre code span")).toHaveCount(0);
  await expect(page.locator("pre")).toHaveCSS("user-select", "text");
  expect(await page.evaluate(() => "previewExecuted" in window)).toBe(false);
});

test("文件增长超过文本上限：显示服务端原因与下载入口", async ({ page }) => {
  await setup(page);
  await page.route("**/files/content?*", (route) =>
    route.fulfill({
      status: 400,
      json: { code: "invalid_request", message: "文件超过 640 KiB 上限" },
    }),
  );
  await row(page, "code.ts").dblclick();
  await expect(page.getByText("文件超过 640 KiB 上限", { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "下载", exact: true })).toBeEnabled();
});

test("续期失败卸载读取元素并释放记录", async ({ page }) => {
  const image = await readFile(new URL("./fixtures/preview-image.png", import.meta.url));
  const server = await previewServer((_url, range, response) =>
    serveBytes(response, image, "image/png", range),
  );
  const { calls } = await setup(page, server.url, true);
  await page.route("**/file-access/*/renew", (route) =>
    route.fulfill({ status: 401, json: { message: "文件访问已过期" } }),
  );
  await page.clock.install();
  await row(page, "photo.jpg").dblclick();
  await expect(page.getByRole("img", { name: "photo.jpg", exact: true })).toBeVisible();
  await page.clock.fastForward(300_000);
  await expect(page.getByText("文件访问已过期", { exact: true })).toBeVisible();
  await expect(page.locator("dialog img")).toHaveCount(0);
  await expect.poll(() => calls.filter((c) => c.method === "DELETE").length).toBe(1);
});
