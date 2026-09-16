//! Linux 的进程图标 —— **尚未实现**，[`available`] 恒为 `false`。
//!
//! 本文件是留给后续实现的接手点。缓存、负缓存、并发合并、预热与补热全部在
//! [`super`] 里，是平台无关的；实现 Linux 只需要把下面两个函数写出来，
//! **不需要动 [`super`] 里的任何一行**。
//!
//! # Linux 上图标不在可执行文件里
//!
//! 这是与 Windows 最根本的差别，也是 [`icon_png`] 的签名必须带 `name` 的原因：
//! ELF 里没有图标资源段，`/usr/bin/firefox` 这个文件本身不含任何图形。
//! 图标住在**桌面条目**与**图标主题**这两套 freedesktop 规范里。
//!
//! # 实现路线
//!
//! 1. **进程名 → 桌面条目**。遍历 `$XDG_DATA_HOME/applications` 与
//!    `$XDG_DATA_DIRS` 各项下的 `applications/*.desktop`（`XDG_DATA_DIRS`
//!    缺省是 `/usr/local/share:/usr/share`），按下面的顺序匹配：
//!    - `StartupWMClass=` 精确等于进程名（最可靠，条目作者就是为这件事写的）；
//!    - `Exec=` 的第一个词（去掉路径与 `%f`/`%U` 这类字段码）等于进程名；
//!    - 文件名去掉 `.desktop` 后等于进程名（兜底，误配率最高）。
//!
//!    规范：Desktop Entry Specification。注意 `NoDisplay=true` 与
//!    `Hidden=true` 的条目要跳过。
//! 2. **桌面条目 → 图标名**。读 `Icon=` 的值。它可能是**绝对路径**
//!    （直接用）也可能是**图标名**（进第 3 步）。
//! 3. **图标名 → 图标文件**。按 Icon Theme Specification 查：当前主题
//!    （`$XDG_DATA_DIRS/icons/<theme>/index.theme` 里的 `Inherits=` 要跟着往上找）
//!    → `hicolor`（规范要求的兜底主题）→ `/usr/share/pixmaps`。
//!    尺寸目录挑最接近 [`super::ICON_SIZE`] 的那一档。
//! 4. **转成 PNG，缩放到 [`super::ICON_SIZE`]**。
//!
//! # SVG 是一笔要先算清楚的账
//!
//! 第 3 步找到的文件有两种可能：
//!
//! - **PNG**：解码、缩放、重新编码即可，`png` crate 已经在依赖里（Windows 侧
//!   引入的，Linux 上要把它移出 `cfg(windows)` 的那一段）。
//! - **SVG**：现代主题里 `scalable/` 目录下全是 SVG，而且不少主题**只有**
//!   SVG。要用它就得栅格化，那意味着引入 `resvg` / `usvg` 这一整套
//!   （连带 `tiny-skia`、字体加载与文本整形）——这是本项目里体量最大的一笔
//!   依赖，远超 `png` 那种只做编码的小库。
//!
//! 可选的折中：只认 PNG，找不到 PNG 就返回 `None`（那台机器上就是没有图标，
//! 与「没装图标主题」的结果一致）。实现时无论选哪条，都要在这里写明理由，
//! 与 `Cargo.toml` 里 `png` 那条注释的要求一样。
//!
//! # 为什么 [`available`] 在服务器上必须为 false
//!
//! Linux 服务器的常态是既没有装任何桌面环境，也就没有 `.desktop` 文件与图标
//! 主题。那种机器上整套预热是纯粹的浪费：每 2 分半钟遍历一遍进程表，对几十个
//! 名字各做一次注定失败的目录查找。[`available`] 就是为这件事存在的闸，
//! 判据建议为：
//!
//! - `$XDG_DATA_DIRS`（缺省 `/usr/local/share:/usr/share`）里**存在**至少一个
//!   `applications` 目录且其中有 `.desktop` 文件；
//! - 且存在至少一个图标主题目录（`<data dir>/icons/hicolor`）。
//!
//! 两条都不满足就返回 `false`，整套调度会自己让开。

use std::path::Path;

/// Linux 上暂时恒为 `false`：见模块文档「为什么 [`available`] 在服务器上必须为
/// false」。
///
/// 返回 `false` 时 [`super`] 完全不起预热与补热任务，端点直接 404。
pub fn available() -> bool {
    false
}

/// 尚未实现，恒为 `None`。
///
/// 实现时用的是 `name`（去 `.desktop` 条目里匹配），`exe` 只在少数情况下有用
/// （`Exec=` 写的是绝对路径时可以直接比对）——与 Windows 恰好相反，
/// 见 [`super`] 的模块文档。
pub fn icon_png(_name: &str, _exe: Option<&Path>) -> Option<Vec<u8>> {
    None
}
