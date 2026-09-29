import { createServer, request as httpRequest, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";

const activeServers = new Set<() => void>();
export function closePreviewServers() {
  for (const close of activeServers) close();
  activeServers.clear();
}

/** 原生下载会绕过 Playwright page.route 重新请求，字节测试必须有真实HTTP响应。 */
export async function previewServer(
  content: (url: string, range: string | undefined, response: ServerResponse) => void,
) {
  const server = createServer((req, res) => {
    if (/\/api\/v1\/file-access\/[^/]+\/content/.test(req.url ?? "")) {
      content(req.url!, req.headers.range, res);
      return;
    }
    const upstream = httpRequest(
      {
        hostname: "127.0.0.1",
        port: 5174,
        path: req.url,
        method: req.method,
        headers: { ...req.headers, host: "127.0.0.1:5174" },
      },
      (reply) => {
        res.writeHead(reply.statusCode ?? 500, reply.headers);
        reply.pipe(res);
      },
    );
    upstream.on("error", () => {
      res.writeHead(502);
      res.end();
    });
    req.pipe(upstream);
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const close = () => {
    server.closeAllConnections();
    server.close();
    activeServers.delete(close);
  };
  activeServers.add(close);
  return {
    url: `http://127.0.0.1:${(server.address() as AddressInfo).port}`,
    close,
  };
}

export function serveBytes(
  response: ServerResponse,
  body: Buffer,
  mime: string,
  range?: string,
  paced = false,
) {
  const start = Number(range?.match(/^bytes=(\d+)-/)?.[1] ?? 0);
  const end = Math.min(Number(range?.match(/-(\d+)$/)?.[1] ?? body.length - 1), body.length - 1);
  response.writeHead(range ? 206 : 200, {
    "Content-Type": mime,
    "Accept-Ranges": "bytes",
    "Content-Length": end - start + 1,
    ...(range ? { "Content-Range": `bytes ${start}-${end}/${body.length}` } : {}),
  });
  if (!paced) {
    response.end(body.subarray(start, end + 1));
    return;
  }
  let offset = start;
  const send = () => {
    const next = Math.min(offset + 32 * 1024, end + 1);
    response.write(body.subarray(offset, next));
    offset = next;
    if (offset > end) {
      clearInterval(timer);
      response.end();
    }
  };
  const timer = setInterval(send, 200);
  response.on("close", () => clearInterval(timer));
  send();
}
