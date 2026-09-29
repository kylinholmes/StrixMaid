import { useEffect, useState } from "react";
import type { components } from "@/api/schema";
import type { Token } from "./highlight";
import s from "./Preview.module.css";
export function TextPreview({
  name,
  data,
}: {
  name: string;
  data: components["schemas"]["FileContent"];
}) {
  const [tokens, setTokens] = useState<Token[] | null>(null);
  useEffect(() => {
    let active = true;
    setTokens(null);
    void import("./highlight")
      .then((module) => module.highlight(name, data.content))
      .then((result) => {
        if (active) setTokens(result);
      })
      .catch(() => {
        /* 懒加载失败保留纯文本。 */
      });
    return () => {
      active = false;
    };
  }, [name, data.content]);
  return (
    <>
      {data.lossy && <p role="status">文件含无效 UTF-8，已用替换字符显示；下载可保留原始字节。</p>}
      {data.truncated && <p role="status">内容已截断，下载可查看完整文件。</p>}
      <pre className={s.text} onContextMenu={(event) => event.stopPropagation()}>
        <code>
          {tokens
            ? tokens.map((token) => (
                <span key={token.offset} data-token={token.kind}>
                  {token.text}
                </span>
              ))
            : data.content}
        </code>
      </pre>
    </>
  );
}
