import { EmptyState } from "@/components";
import s from "./PageChrome.module.css";

/** 尚未实现的页面：外壳先立起来，页面按 spec §12 的顺序逐个做成成品。 */
export function StubPage({ title }: { title: string }) {
  return (
    <>
      <header className={s.head}>
        <h1>{title}</h1>
      </header>
      <div className={s.body}>
        <EmptyState title="尚未实现" />
      </div>
    </>
  );
}
