import { type Dispatch, type SetStateAction, useEffect, useState } from "react";

/**
 * 「点哪都关」的开合状态。打开期间监听 document click,任意点击即关;
 * 触发按钮自己要 `e.stopPropagation()`,否则开的那一下就被这里关掉。
 */
export function useDismiss(): [boolean, Dispatch<SetStateAction<boolean>>] {
  const [open, setOpen] = useState(false);
  useEffect(() => {
    if (!open) return;
    const close = () => setOpen(false);
    document.addEventListener("click", close);
    return () => document.removeEventListener("click", close);
  }, [open]);
  return [open, setOpen];
}
