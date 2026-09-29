export const MAX_TEXT_BYTES = 640 * 1024;
export type PreviewKind = "text" | "image" | "pdf" | "audio" | "video" | "unsupported";
export function extension(name: string): string {
  return name.split(".").at(-1)?.toLowerCase() ?? "";
}
export function previewKind(name: string): PreviewKind {
  const ext = extension(name);
  if (/^(png|jpe?g|gif|webp|bmp|heic|heif)$/.test(ext)) return "image";
  if (ext === "pdf") return "pdf";
  if (/^(mp3|wav|ogg|oga|flac|m4a|aac|opus)$/.test(ext)) return "audio";
  if (/^(mp4|webm|ogv|mov|m4v|mkv)$/.test(ext)) return "video";
  if (
    /^(txt|md|mdx|log|csv|tsv|json|jsonc|xml|yaml|yml|toml|ini|conf|cfg|env|js|jsx|mjs|cjs|ts|tsx|py|rs|go|c|h|cpp|hpp|java|css|scss|sql|sh|bash|zsh|ps1|html?|svg)$/.test(
      ext,
    ) ||
    /^(readme|license|makefile|dockerfile|hosts|\.gitignore)$/i.test(name)
  )
    return "text";
  return "unsupported";
}
