import { useCallback, useEffect, useRef, useState } from "react";
import s from "./Workspace.module.css";

/**
 * 自绘的悬浮滚动条（负责人定：原生右侧滚动条不好看，换成自己的）。
 * 滚动时浮现、空闲淡出、可拖拽；宿主容器把原生滚动条藏掉（CSS），
 * 滚动本身仍是原生的——这里只画那根「拇指」。
 */
export function OverlayScrollbar({ target }: { target: React.RefObject<HTMLElement | null> }) {
  const [thumb, setThumb] = useState<{ top: number; height: number } | null>(null);
  const [visible, setVisible] = useState(false);
  const [dragging, setDragging] = useState(false);
  const hideTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const update = useCallback(() => {
    const el = target.current;
    if (!el) return;
    const { scrollHeight, clientHeight, scrollTop } = el;
    if (scrollHeight <= clientHeight + 1) {
      setThumb(null);
      return;
    }
    const height = Math.max(24, (clientHeight / scrollHeight) * clientHeight);
    const top = (scrollTop / (scrollHeight - clientHeight)) * (clientHeight - height);
    setThumb({ top, height });
  }, [target]);

  const poke = useCallback(() => {
    setVisible(true);
    if (hideTimer.current) clearTimeout(hideTimer.current);
    hideTimer.current = setTimeout(() => setVisible(false), 900);
  }, []);

  useEffect(() => {
    const el = target.current;
    if (!el) return;
    const onScroll = () => {
      update();
      poke();
    };
    el.addEventListener("scroll", onScroll, { passive: true });
    const ro = new ResizeObserver(update);
    ro.observe(el);
    // 内容高度变化（翻页加载、换目录）不触发容器 resize，用 MutationObserver 兜住。
    const mo = new MutationObserver(update);
    mo.observe(el, { childList: true, subtree: true });
    update();
    return () => {
      el.removeEventListener("scroll", onScroll);
      ro.disconnect();
      mo.disconnect();
      if (hideTimer.current) clearTimeout(hideTimer.current);
    };
  }, [target, update, poke]);

  const dragStart = (e: React.PointerEvent<HTMLDivElement>) => {
    const el = target.current;
    if (!el || !thumb) return;
    e.preventDefault();
    const grip = e.currentTarget;
    grip.setPointerCapture(e.pointerId);
    setDragging(true);
    const startY = e.clientY;
    const startTop = el.scrollTop;
    const ratio = (el.scrollHeight - el.clientHeight) / (el.clientHeight - thumb.height);
    const move = (ev: PointerEvent) => {
      el.scrollTop = startTop + (ev.clientY - startY) * ratio;
    };
    const end = () => {
      setDragging(false);
      grip.removeEventListener("pointermove", move);
      grip.removeEventListener("pointerup", end);
      grip.removeEventListener("pointercancel", end);
    };
    grip.addEventListener("pointermove", move);
    grip.addEventListener("pointerup", end);
    grip.addEventListener("pointercancel", end);
  };

  if (!thumb) return null;
  return (
    <div
      className={s.overlayThumb}
      style={{ top: thumb.top, height: thumb.height }}
      data-visible={visible || dragging || undefined}
      data-dragging={dragging || undefined}
      onPointerDown={dragStart}
      aria-hidden
    />
  );
}
