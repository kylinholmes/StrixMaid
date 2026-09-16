import { useEffect, useState } from "react";
import { ensureSvcIcon, peekSvcIcon } from "./icon";
import s from "./Svc.module.css";

/**
 * 订阅某个服务的图标。
 *
 * 结果在**渲染时同步**从模块级缓存读取：unit 列表会被 WS 推送与兜底重拉反复重渲，
 * 若每次都从空白开始异步加载，图标会跟着刷新一起闪。缓存命中时首帧即是最终值，
 * 只有真正需要发请求的那一次才会多一次重渲。
 */
export function useSvcIcon(name: string): string | null {
  const [, bump] = useState(0);
  useEffect(() => {
    let alive = true;
    const before = peekSvcIcon(name);
    void ensureSvcIcon(name).then(() => {
      // 结论没变就不重渲
      if (alive && peekSvcIcon(name) !== before) bump((n) => n + 1);
    });
    return () => {
      alive = false;
    };
  }, [name]);
  return peekSvcIcon(name);
}

/**
 * 服务名前的图标：有自己的就用自己的，没有就用通用齿轮，两者都没有就留空。
 *
 * 容器尺寸固定，图标到达前后行高与列宽都不变，表格不会因为图标陆续加载而抖动。
 * 大部分服务（本机 317 个里有 300 个）没有自己的图标，显示的是齿轮——与
 * `services.msc` 一致；非 Windows 平台上两个端点都不存在，那一列恒为空白。
 * 图标纯装饰，服务名就在旁边，对读屏器隐藏。
 */
export function SvcIcon({ name }: { name: string }) {
  const src = useSvcIcon(name);
  return (
    <span className={s.icon} aria-hidden="true">
      {/* key 用名字：列表刷新时名字不变，<img> 就被复用，不重建也就不闪 */}
      {src !== null && <img key={name} className={s.iconImg} src={src} alt="" />}
    </span>
  );
}
