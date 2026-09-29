import { useEffect, useRef, useState } from "react";
import { api } from "@/api/client";
import type { components } from "@/api/schema";
import { sessionSignal } from "@/session/lifecycle";
import { AccessLease } from "./access";
import type { PreviewTarget } from "./FilePreview";
import { MAX_TEXT_BYTES, previewKind } from "./kind";
import s from "./Preview.module.css";
import { TextPreview } from "./TextPreview";

type State =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "text"; data: components["schemas"]["FileContent"] }
  | { kind: "image" | "pdf" | "audio" | "video"; url: string };

export function PreviewBody({ target }: { target: PreviewTarget }) {
  const { path, entry, reason } = target;
  const [state, setState] = useState<State>({ kind: "loading" });
  const activeLease = useRef<AccessLease | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    const signal = AbortSignal.any([controller.signal, sessionSignal()]);
    let lease: AccessLease | undefined;
    const fail = (message: string) => setState({ kind: "error", message });
    const load = async () => {
      const kind = previewKind(entry.name);
      if (reason) return fail(reason);
      if (kind === "unsupported") return fail("此文件类型不支持预览，可以下载后打开。");
      if (kind === "text") {
        if (entry.size_bytes > MAX_TEXT_BYTES)
          return fail("文本超过 640 KiB 预览上限，请下载查看。");
        const { data, error } = await api.GET("/api/v1/files/content", {
          params: { query: { path } },
          signal,
        });
        if (error) throw new Error(error.message);
        if (!signal.aborted) setState({ kind: "text", data });
      } else {
        // preview 用途由 worker 生成最长边 1600px、已转正的图片档。
        lease = new AccessLease();
        activeLease.current = lease;
        const access = await lease.open(path, "preview");
        if (signal.aborted) return;
        setState({ kind, url: access.url });
        lease.renew((error) => {
          if (!signal.aborted) fail(error.message);
        });
      }
    };
    void load().catch((error: unknown) => {
      lease?.dispose();
      if (!signal.aborted) fail(error instanceof Error ? error.message : "预览失败，请下载查看。");
    });
    return () => {
      controller.abort();
      lease?.dispose();
      activeLease.current = null;
    };
  }, [path, entry.name, entry.size_bytes, reason]);
  const unavailable = () => {
    activeLease.current?.dispose();
    setState({ kind: "error", message: "浏览器无法预览此格式或文件读取失败，请下载查看。" });
  };
  return (
    <div className={s.body}>
      {state.kind === "loading" && <p role="status">正在读取预览…</p>}
      {state.kind === "error" && <p role="status">{state.message}</p>}
      {state.kind === "text" && <TextPreview name={entry.name} data={state.data} />}
      {state.kind === "image" && (
        <img
          className={s.image}
          src={state.url}
          alt={entry.name}
          referrerPolicy="no-referrer"
          onError={unavailable}
        />
      )}
      {state.kind === "pdf" && (
        <>
          <p>若浏览器无法显示 PDF，请使用下载按钮。</p>
          <iframe
            title={`PDF 预览：${entry.name}`}
            className={s.pdf}
            src={state.url}
            sandbox=""
            referrerPolicy="no-referrer"
            onError={unavailable}
          />
        </>
      )}
      {state.kind === "audio" && (
        <audio
          aria-label={entry.name}
          controls
          preload="metadata"
          src={state.url}
          onError={unavailable}
        >
          <track kind="captions" />
        </audio>
      )}
      {state.kind === "video" && (
        <video
          aria-label={entry.name}
          className={s.video}
          controls
          preload="metadata"
          src={state.url}
          onError={unavailable}
        >
          <track kind="captions" />
        </video>
      )}
    </div>
  );
}
