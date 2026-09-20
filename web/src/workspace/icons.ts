/**
 * 内置文件类型图标（roadmap/12 §4.7）：文件夹优先 Papirus、文件优先
 * vscode-icons，两套互补、零请求——全部按名字/扩展名在前端解决。
 * 资产与许可证见 `src/assets/icons/`（NOTICE.txt）。
 *
 * 映射刻意保守：认不出的返回 `null`，由调用方回落到通用图标——
 * 错的图标比没有图标更误导。
 */

/** Vite 在构建期收集全部图标的最终 URL（哈希名）。 */
const urls = import.meta.glob("../assets/icons/{folders,files}/*.svg", {
  eager: true,
  query: "?url",
  import: "default",
}) as Record<string, string>;

function url(group: "folders" | "files", slug: string): string | null {
  return urls[`../assets/icons/${group}/${slug}.svg`] ?? null;
}

/** 目录名（小写）→ 文件夹图标 slug。 */
const FOLDER_BY_NAME: Record<string, string> = {
  // 用户目录（Papirus 的强项）
  desktop: "folder-desktop",
  downloads: "folder-download",
  download: "folder-download",
  documents: "folder-docs",
  pictures: "folder-images",
  images: "folder-images",
  photos: "folder-images",
  videos: "folder-video",
  movies: "folder-video",
  music: "folder-audio",
  audio: "folder-audio",
  ".trash": "folder-trash",
  // 服务器目录
  etc: "folder-config",
  config: "folder-config",
  ".config": "folder-config",
  "conf.d": "folder-config",
  log: "folder-log",
  logs: "folder-log",
  tmp: "folder-temp",
  temp: "folder-temp",
  backup: "folder-backup",
  backups: "folder-backup",
  ".ssh": "folder-keys",
  ssl: "folder-keys",
  certs: "folder-keys",
  private: "folder-keys",
  scripts: "folder-scripts",
  bin: "folder-scripts",
  db: "folder-database",
  database: "folder-database",
  mysql: "folder-database",
  postgres: "folder-database",
  postgresql: "folder-database",
  srv: "folder-server",
  server: "folder-server",
  www: "folder-nginx",
  html: "folder-nginx",
  nginx: "folder-nginx",
  docker: "folder-docker",
  ".git": "folder-git",
  src: "folder-src",
  node_modules: "folder-node",
};

/** 扩展名（小写、不含点）→ 文件图标 slug。 */
const FILE_BY_EXT: Record<string, string> = {
  txt: "document",
  text: "document",
  md: "markdown",
  markdown: "markdown",
  pdf: "pdf",
  doc: "word",
  docx: "word",
  odt: "word",
  xls: "table",
  xlsx: "table",
  ods: "table",
  csv: "csv",
  tsv: "csv",
  png: "image",
  jpg: "image",
  jpeg: "image",
  gif: "image",
  webp: "image",
  bmp: "image",
  ico: "image",
  svg: "image",
  avif: "image",
  mp3: "audio",
  flac: "audio",
  wav: "audio",
  ogg: "audio",
  m4a: "audio",
  mp4: "video",
  mkv: "video",
  webm: "video",
  mov: "video",
  avi: "video",
  zip: "zip",
  gz: "zip",
  tgz: "zip",
  bz2: "zip",
  xz: "zip",
  zst: "zip",
  tar: "zip",
  "7z": "zip",
  rar: "zip",
  deb: "zip",
  rpm: "zip",
  exe: "exe",
  msi: "exe",
  bin: "exe",
  appimage: "exe",
  sh: "console",
  bash: "console",
  zsh: "console",
  fish: "console",
  ps1: "console",
  bat: "console",
  cmd: "console",
  json: "json",
  jsonc: "json",
  yml: "yaml",
  yaml: "yaml",
  toml: "toml",
  ini: "toml",
  conf: "toml",
  cfg: "toml",
  xml: "xml",
  plist: "xml",
  sql: "database",
  sqlite: "database",
  db: "database",
  log: "log",
  pem: "key",
  crt: "key",
  cer: "key",
  key: "key",
  pub: "key",
  service: "systemd",
  socket: "systemd",
  timer: "systemd",
  mk: "makefile",
  js: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  jsx: "javascript",
  ts: "typescript",
  tsx: "typescript",
  mts: "typescript",
  rs: "rust",
  py: "python",
  pyw: "python",
  go: "go",
  c: "c",
  h: "c",
  java: "java",
  jar: "java",
  html: "html",
  htm: "html",
  css: "css",
  scss: "css",
  less: "css",
};

/** 完整文件名（小写）→ slug，优先于扩展名。 */
const FILE_BY_NAME: Record<string, string> = {
  dockerfile: "docker",
  "docker-compose.yml": "docker",
  "docker-compose.yaml": "docker",
  "compose.yml": "docker",
  "compose.yaml": "docker",
  makefile: "makefile",
  gnumakefile: "makefile",
  "nginx.conf": "nginx",
  "cargo.toml": "toml",
};

/** 目录的图标 URL；总有值（认不出回落到普通文件夹）。 */
export function folderIconUrl(name: string): string {
  const slug = FOLDER_BY_NAME[name.toLowerCase()] ?? "folder-base";
  return url("folders", slug) ?? (url("folders", "folder-base") as string);
}

/** 文件的图标 URL；认不出返回 `null`（调用方回落到通用图标）。 */
export function fileIconUrl(name: string): string | null {
  const lower = name.toLowerCase();
  const byName = FILE_BY_NAME[lower];
  if (byName) return url("files", byName);
  const dot = lower.lastIndexOf(".");
  if (dot <= 0) return null;
  const slug = FILE_BY_EXT[lower.slice(dot + 1)];
  return slug ? url("files", slug) : null;
}
