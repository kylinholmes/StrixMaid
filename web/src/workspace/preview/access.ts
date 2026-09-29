import { authHeaders } from "@/api/client";
import type { components } from "@/api/schema";
import { onSessionReset, sessionSignal } from "@/session/lifecycle";

export type FileAccess = components["schemas"]["FileAccessResponse"];
export type Purpose = components["schemas"]["FileAccessPurpose"];
type FileAccessRequest = components["schemas"]["FileAccessRequest"];
export const RENEW_MS = 5 * 60_000;

export async function responseError(response: Response): Promise<Error> {
  const body = await response.json().catch(() => null);
  return new Error(body?.message ?? `文件请求失败（${response.status}）`);
}

function validateAccess(value: FileAccess): FileAccess {
  // 原生元素只接收本源文件读端点，绝不接受可执行 URL 或外部地址。
  if (
    typeof value.id !== "string" ||
    !value.id ||
    value.url !== `/api/v1/file-access/${encodeURIComponent(value.id)}/content` ||
    !Number.isFinite(value.expires_in_secs) ||
    value.expires_in_secs <= 0
  )
    throw new Error("文件访问响应无效");
  return value;
}

/** 每条记录捕获所属会话凭据；换账号后的回收不能误用新用户的 Bearer。 */
export class AccessLease {
  private headers = authHeaders();
  private controller = new AbortController();
  private scope = sessionSignal();
  private access: FileAccess | null = null;
  private disposed = false;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private unsubscribe: () => void;

  constructor() {
    this.unsubscribe = onSessionReset(() => this.dispose());
  }

  private async request(path: string, body?: unknown): Promise<FileAccess> {
    if (!this.headers) throw new Error("请重新登录后打开文件");
    // StrictMode 的挂载检查会同步清理首轮 effect；不为已关闭面板创建记录。
    await Promise.resolve();
    this.controller.signal.throwIfAborted();
    this.scope.throwIfAborted();
    const response = await fetch(path, {
      method: "POST",
      credentials: "same-origin",
      headers: { ...this.headers, "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.any([this.controller.signal, this.scope, AbortSignal.timeout(30_000)]),
    });
    if (!response.ok) throw await responseError(response);
    return validateAccess(await response.json());
  }

  async open(path: string, purpose: Purpose): Promise<FileAccess> {
    const body: FileAccessRequest = { path, purpose };
    const access = await this.request("/api/v1/file-access", body);
    this.access = access;
    if (this.disposed || this.scope.aborted) {
      this.release(access.id);
      throw new DOMException("预览已关闭", "AbortError");
    }
    return access;
  }

  /** 只在面板存活时续期；失败立即卸载原生读取元素。 */
  renew(onError: (error: Error) => void): void {
    this.timer = setTimeout(async () => {
      if (this.disposed || !this.access) return;
      try {
        const renewed = await this.request(
          `/api/v1/file-access/${encodeURIComponent(this.access.id)}/renew`,
        );
        if (this.disposed) return;
        if (renewed.id !== this.access.id || renewed.url !== this.access.url)
          throw new Error("续期响应无效");
        this.access = renewed;
        this.renew(onError);
      } catch (error) {
        if (!this.disposed) {
          this.dispose();
          onError(error instanceof Error ? error : new Error("文件访问续期失败"));
        }
      }
    }, RENEW_MS);
  }

  /** 原生下载没有浏览器完成回调，保留到记录自然过期；关预览不会中断下载。 */
  expireAfter(seconds: number): void {
    this.timer = setTimeout(() => this.dispose(), seconds * 1000);
  }

  private release(id: string): void {
    if (!this.headers) return;
    void fetch(`/api/v1/file-access/${encodeURIComponent(id)}`, {
      method: "DELETE",
      headers: this.headers,
      credentials: "same-origin",
      signal: AbortSignal.timeout(5_000),
      keepalive: true,
    }).catch(() => undefined);
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.controller.abort();
    clearTimeout(this.timer);
    this.unsubscribe();
    if (this.access) this.release(this.access.id);
    this.access = null;
  }
}

export async function downloadFile(path: string, name: string): Promise<void> {
  const lease = new AccessLease();
  try {
    const access = await lease.open(path, "download");
    const anchor = document.createElement("a");
    anchor.href = access.url;
    anchor.download = name;
    anchor.referrerPolicy = "no-referrer";
    document.body.append(anchor);
    anchor.click();
    anchor.remove();
    lease.expireAfter(access.expires_in_secs);
  } catch (error) {
    lease.dispose();
    throw error;
  }
}
