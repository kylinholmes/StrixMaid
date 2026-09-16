import { useEffect, useState } from "react";
import { ensureIcon, peekIcon } from "./icon";
import s from "./Proc.module.css";

/**
 * 订阅某个映像名的图标。
 *
 * 结果在**渲染时同步**从模块级缓存读取：进程列表是轮询刷新的，行会被反复重渲，
 * 若每次都从空白开始异步加载，图标会跟着刷新一起闪。缓存命中时首帧即是最终值，
 * 只有真正需要发请求的那一次才会多一次重渲。
 */
export function useProcIcon(name: string): string | null {
  const [, bump] = useState(0);
  useEffect(() => {
    let alive = true;
    const before = peekIcon(name);
    void ensureIcon(name).then(() => {
      // 结论没变就不重渲（负缓存命中时最常见）
      if (alive && peekIcon(name) !== before) bump((n) => n + 1);
    });
    return () => {
      alive = false;
    };
  }, [name]);
  return peekIcon(name);
}

/**
 * 进程名前的程序图标。
 *
 * 容器尺寸固定，图标到达前后行高与列宽都不变，表格不会因为图标陆续加载而抖动。
 * 取不到就让容器空着：系统进程（services.exe、csrss.exe 等）非管理员下取不到图标，
 * 非 Windows 平台上这个端点根本不存在——空白是常态而不是错误，因此既不显示碎图标，
 * 也不放警示性的占位符。图标纯装饰，进程名就在旁边，对读屏器隐藏。
 */
export function ProcIcon({ name }: { name: string }) {
  const src = useProcIcon(name);
  return (
    <span className={s.icon} aria-hidden="true">
      {/* key 用名字：轮询刷新时名字不变，<img> 就被复用，不重建也就不闪 */}
      {src !== null && <img key={name} className={s.iconImg} src={src} alt="" />}
    </span>
  );
}
