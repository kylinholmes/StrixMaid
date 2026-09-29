import { useEffect, useId, useRef, useState } from "react";
import { Button } from "@/components";
import { fmtBytes } from "@/lib/fmt";
import type { DirEntry } from "../useDirListing";
import { downloadFile } from "./access";
import s from "./Preview.module.css";
import { PreviewBody } from "./PreviewBody";

export interface PreviewTarget {
  path: string;
  entry: DirEntry;
  reason?: string;
}
export function FilePreview({ target, onClose }: { target: PreviewTarget; onClose: () => void }) {
  const ref = useRef<HTMLDialogElement>(null);
  const title = useId();
  const [downloading, setDownloading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    const focus = document.activeElement;
    const dialog = ref.current;
    dialog?.showModal();
    return () => {
      dialog?.close();
      if (focus instanceof HTMLElement && focus.isConnected) focus.focus();
    };
  }, []);
  return (
    <dialog
      ref={ref}
      className={s.dialog}
      aria-labelledby={title}
      onCancel={(event) => {
        event.preventDefault();
        onClose();
      }}
    >
      <header className={s.header}>
        <div>
          <h2 id={title}>{target.entry.name}</h2>
          <span>{fmtBytes(target.entry.size_bytes)}</span>
        </div>
        <Button
          disabled={!!target.reason || downloading}
          onClick={() => {
            setDownloading(true);
            setError(null);
            void downloadFile(target.path, target.entry.name)
              .catch((err: unknown) => {
                setError(err instanceof Error ? err.message : "下载失败");
              })
              .finally(() => setDownloading(false));
          }}
        >
          {downloading ? "准备下载…" : "下载"}
        </Button>
        <Button onClick={onClose} aria-label="关闭预览">
          关闭
        </Button>
      </header>
      {error && <p role="alert">{error}</p>}
      <PreviewBody key={target.path} target={target} />
    </dialog>
  );
}
