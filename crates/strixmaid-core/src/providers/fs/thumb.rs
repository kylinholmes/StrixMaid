//! 缩略图：在 **worker 内**把一张图变成一张小图（roadmap/12 §4.7）。
//!
//! # 这里推翻了一条早先的决定，理由写在这
//!
//! §4.7 原本定的是「后端只送字节，浏览器自己解码——worker 里不进任何图片
//! 解码库，图片解析器历来是 CVE 重灾区」，并为此给缩略图设了 8 MiB 的源文件
//! 上限。**实测下来这条规则在真实照片目录上等于没有缩略图**：相机直出的 JPG
//! 普遍 13～27 MiB，一个都过不了闸；而那恰恰是最需要预览的地方。
//!
//! 负责人 2026-09-21 据此改判：**服务端出真缩略图**。原先的顾虑没有消失，
//! 换成按三层压：
//!
//! | 层 | 做法 | 消掉的风险 |
//! |---|---|---|
//! | 一 | 相机 JPEG 先取 **EXIF 内嵌预览**（`embedded_jpeg`） | 这条路**一个像素都不解码**，只按 TIFF 目录结构找偏移量；照片目录的绝大多数条目走这里 |
//! | 二 | 真要解码时**先读文件头拿尺寸**，像素数超 `MAX_PIXELS` 直接拒绝 | 「一张 100000×100000 的 PNG 撑爆内存」这类解压炸弹 |
//! | 三 | 解码器是纯 Rust（zune-jpeg / png / image-webp），且整段包在 [`std::panic::catch_unwind`] 里 | debug / release 均为 unwind，捕获 Rust panic；不承诺恢复 OOM 或外部库进程终止 |
//!
//! 另外两点没变，仍然是承重的：解码发生在 **worker**（登录用户身份），所以
//! 「能不能读这个文件」仍由文件权限裁决，主进程一行判断都不写；
//! `allowed_roots` 的校验也照旧在 `super::resolve` 里。
//!
//! # 为什么内嵌预览值得单独一条路
//!
//! 不只是省 CPU——它**完全不经过解码器**，也就完全不暴露上面那类风险面。
//! 相机与手机拍的 JPEG 几乎都在 EXIF 里放了一张 160×120 上下的预览图，
//! 找到它只需要按 TIFF 的 IFD 结构读几个整数（`embedded_jpeg` 全程只做
//! 边界检查与整数读取，不碰像素）。取出来的那张仍然交给浏览器解码，
//! 与原方案的信任边界一致。
//!
//! 代价是内嵌预览的尺寸由相机决定（常见 160×120），比我们要的 256 小。
//! 小了就**放着不管**——放大只会糊，且平铺格子是 68 CSS 像素，160 够用。
//!
//! # 输出格式
//!
//! 有 alpha 通道的编 PNG（丢了透明会在深色主题下出现白底方块），
//! 其余编 JPEG（照片编 PNG 会大十几倍）。两者都远小于
//! [`FS_THUMB_MAX_BYTES`]；真超了报错而不是把控制面顶住。

use std::io::Cursor;
use std::path::Path;

use image::{ImageDecoder, ImageFormat, ImageReader};
use strixmaid_types::rpc::{FS_THUMB_MAX_BYTES, FS_THUMB_MAX_PX, FsThumb};
use strixmaid_types::{ApiError, ApiResult};

use super::io_err;

static DECODE_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// 门禁在进入阻塞线程池之前执行，permit 随真实工作一起结束，不能随请求取消释放。
pub(super) async fn render(file: std::path::PathBuf, max_px: u32) -> ApiResult<FsThumb> {
    decode_job(move || thumb_blocking(&file, max_px)).await
}

/// 缩略图与大图预览共用同一份解码配额。
pub(super) async fn decode_job<T: Send + 'static>(
    work: impl FnOnce() -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    bounded_decode(&DECODE_SLOTS, work).await
}

async fn bounded_decode<T: Send + 'static>(
    slots: &'static tokio::sync::Semaphore,
    work: impl FnOnce() -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    let permit = slots.try_acquire().map_err(|_| {
        ApiError::new(
            strixmaid_types::ErrorCode::Unavailable,
            "缩略图解码繁忙，请稍后重试",
        )
    })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
            .map_err(|_| ApiError::invalid_request("图片解码失败"))?
    })
    .await
    .map_err(|e| ApiError::internal("fs.thumb 任务异常").with_detail(e.to_string()))?
}

/// 允许解码的像素总数上限（宽 × 高）。
///
/// 8000 万像素：现役最高像素的消费级相机（1 亿像素的中画幅除外）都在这之下，
/// 而按 RGBA 算它已经是 320 MiB 的解码缓冲——再往上就该拒绝而不是硬扛。
/// 这个判断在**解码之前**做：`ImageReader` 只读文件头就能给出尺寸。
pub(super) const MAX_PIXELS: u64 = 80_000_000;

/// 源文件大小上限。
///
/// 与旧的 8 MiB 上限不是一回事：那时是「整份下发给浏览器」，所以按带宽定；
/// 现在字节不出机器，这个上限只防「拿一个几 GiB 的文件让 worker 去读头」。
pub(super) const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
pub(super) const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;

/// JPEG 输出质量。80 在 256 px 这个尺度上看不出与 95 的差别，体积却只有一半。
const JPEG_QUALITY: u8 = 80;

/// 取小写扩展名。
fn ext_of(file: &Path) -> String {
    file.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// 纯 Rust 解码器认得的扩展名。认不出的不尝试解码——省得把一个 `.bin`
/// 喂进解码器去试格式。
pub(super) fn rust_decodable(file: &Path) -> bool {
    matches!(
        ext_of(file).as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp"
    )
}

/// **只有 macOS**：借系统解码器（ImageIO，经 [`crate::platform::appkit`]）
/// 处理纯 Rust 确实解不了的格式。目前只有 HEIC / HEIF 一类。
///
/// # 为什么对 HEIC 破一次例
///
/// HEIC 的图像项是 **HEVC 编码**的，而且容器里没有可捡的现成预览——实测
/// Apple 的 HEIC 只有 `hvc1` 瓦片加一个 `grid`，连 JPEG 编码的项都没有，
/// 所以 JPEG 那条「零解码」的便宜路在这里不存在。纯 Rust 生态至今没有
/// HEVC 解码器（AV1 有 rav1d，HEVC 没有——能力不是问题，专利格局把生态的
/// 投入都改道去了 AV1）。可选项只有三个：不支持、引 libheif（C++ 的 HEVC
/// 解码器，正是三层防线要挡的那一类，还会破坏 musl 静态链接）、或者借系统的。
///
/// 借系统这条**风险最小**：ImageIO 是 C 写的、历史上有过 CVE，这点不隐瞒，
/// 但它是 Finder、预览、快速查看在**同一批文件**上天天跑的同一个解码器——
/// 用户的 Mac 本来就在解这些文件，我们没有引入一个新的攻击面，只是复用了
/// 已经在那儿的那个。而 libheif 是往进程里请一个**新的** C++ 解码器。
///
/// 仍然只对 HEIC 开：别的格式纯 Rust 解得了，就走纯 Rust。
fn system_decodable(file: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        matches!(ext_of(file).as_str(), "heic" | "heif") && crate::platform::appkit::available()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = file;
        false
    }
}

/// 本机能不能给这个文件出缩略图（两条路任一认得即可）。
pub fn supported(file: &Path) -> bool {
    rust_decodable(file) || system_decodable(file)
}

/// 出一张缩略图。同步、阻塞，调用方负责 `spawn_blocking`。
pub fn thumb_blocking(file: &Path, max_px: u32) -> ApiResult<FsThumb> {
    let max_px = max_px.clamp(16, FS_THUMB_MAX_PX);

    let meta = std::fs::metadata(file).map_err(|e| io_err(file, &e))?;
    if meta.is_dir() {
        return Err(ApiError::invalid_request(format!(
            "{} 是目录，不是图片",
            file.display()
        )));
    }
    if meta.len() > MAX_SOURCE_BYTES {
        return Err(ApiError::invalid_request(format!(
            "文件过大（{} 字节），不出缩略图",
            meta.len()
        )));
    }
    if !supported(file) {
        return Err(ApiError::invalid_request(format!(
            "{} 不是支持的图片格式",
            file.display()
        )));
    }

    // 纯 Rust 解不了、但系统解得了的（macOS 的 HEIC）：整个文件交给系统
    // 解码器，拿回像素再走本模块统一的编码判据。ImageIO 自己会应用方向，
    // 所以这里报 1（正立）——再转一次就转过头了。
    if !rust_decodable(file) {
        return system_thumb(file, max_px);
    }

    // take 防止 metadata 检查后文件继续增长，不能只凭检查时的长度分配。
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .map_err(|e| io_err(file, &e))?
        .take(MAX_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_err(file, &e))?;
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err(ApiError::invalid_request("图片源文件超过读取上限"));
    }
    // 方向标签对两条路都适用：相机不转像素，只在 EXIF 里记一句要转多少度。
    let orientation = exif_orientation(&bytes);

    // 第一条路：EXIF 内嵌预览，不解码。
    if let Some(embedded) = embedded_jpeg(&bytes)
        && let Some((w, h)) = jpeg_size(embedded)
        && embedded.len() <= FS_THUMB_MAX_BYTES
        // 内嵌预览偶尔是整张原图（某些厂商这么干），那就没有意义，
        // 交给下面真的缩一张。
        && w.max(h) <= FS_THUMB_MAX_PX * 2
    {
        return Ok(FsThumb {
            mime: "image/jpeg".to_owned(),
            width: w,
            height: h,
            orientation,
            embedded: true,
            data_hex: hex::encode(embedded),
        });
    }

    decode_and_scale(&bytes, max_px, orientation)
}

/// 借系统解码器出缩略图（见 [`system_decodable`]）。
///
/// 只有解码这一步借系统，缩放由系统顺手做了（它画进多大的位图就是多大），
/// **编码仍走 [`encode_thumb`]**——PNG 还是 JPEG 只有一处判据。
fn system_thumb(file: &Path, max_px: u32) -> ApiResult<FsThumb> {
    #[cfg(target_os = "macos")]
    {
        let path = file.to_str().ok_or_else(|| {
            ApiError::invalid_request(format!("{} 的路径不是合法 UTF-8", file.display()))
        })?;
        let (w, h, rgba) = crate::platform::appkit::decode_rgba(path, max_px).ok_or_else(|| {
            ApiError::invalid_request(format!("{} 系统解码器也解不了", file.display()))
        })?;
        let img = image::RgbaImage::from_raw(w, h, rgba)
            .ok_or_else(|| ApiError::internal("系统解码器给出的像素缓冲与尺寸对不上"))?;
        // ImageIO 解码时已经把 EXIF 方向应用过了，这里报正立。
        encode_thumb(&image::DynamicImage::ImageRgba8(img), 1)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = max_px;
        Err(ApiError::invalid_request(format!(
            "{} 不是支持的图片格式",
            file.display()
        )))
    }
}

/// 解码 + 缩放 + 编码；并发配额与 panic 恢复在阻塞任务入口统一执行。
fn decode_and_scale(bytes: &[u8], max_px: u32, orientation: u8) -> ApiResult<FsThumb> {
    encode_thumb(&decode_scaled(bytes, max_px, false)?, orientation)
}

pub(super) fn decode_scaled(
    bytes: &[u8],
    max_px: u32,
    orient: bool,
) -> ApiResult<image::DynamicImage> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| ApiError::invalid_request(format!("认不出图片格式：{e}")))?;
    let format = reader.format();

    // 解码**之前**先拿尺寸：这一步只读文件头，是解压炸弹的闸。
    let (w, h) = reader
        .into_dimensions()
        .map_err(|e| ApiError::invalid_request(format!("读不出图片尺寸：{e}")))?;
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(ApiError::invalid_request(format!(
            "图片像素过多（{w}×{h}），不出缩略图"
        )));
    }

    // 重新建一个 reader：尺寸那一步把上一个消费掉了。
    let mut reader = ImageReader::new(Cursor::new(bytes));
    if let Some(f) = format {
        reader.set_format(f);
    }
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| ApiError::invalid_request(format!("图片解码失败：{e}")))?;
    // 与 ImageReader::decode 一样，先为输出像素预留预算，再把剩余额度
    // 交给解码器；直接 from_decoder 本身不会扣除这笔分配。
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits
        .reserve(decoder.total_bytes())
        .and_then(|()| decoder.set_limits(limits))
        .map_err(|e| ApiError::invalid_request(format!("图片超过解码内存上限：{e}")))?;
    let orientation = if orient {
        decoder
            .orientation()
            .map_err(|e| ApiError::invalid_request(format!("图片方向无效：{e}")))?
    } else {
        image::metadata::Orientation::NoTransforms
    };
    let img = image::DynamicImage::from_decoder(decoder)
        .map_err(|e| ApiError::invalid_request(format!("图片解码失败：{e}")))?;

    // `thumbnail` 是为缩略图准备的快速路径（先整数倍降采样再精修），
    // 比 `resize` 快一个量级，在 256 px 这个尺度上看不出差别。
    //
    // 本来就比目标小的图**原样保留**：`thumbnail` 会把它放大（按长边填满），
    // 放大只会糊，还白白把一张 2 KB 的图标编成几十 KB。
    let mut small = if w <= max_px && h <= max_px {
        img
    } else {
        img.thumbnail(max_px, max_px)
    };
    small.apply_orientation(orientation);
    Ok(small)
}

/// 缩好的图 → `FsThumb`。**格式判据只有这一处**，两条解码路共用。
///
/// 判据不是「声明里有没有 alpha 通道」而是「**真的用到了没有**」：
/// 系统解码器一律给 RGBA，截图导出的 PNG 也常是全不透明的 RGBA——
/// 按声明判会把一张 20 KB 的照片编成 150 KB 的 PNG。
fn encode_thumb(img: &image::DynamicImage, orientation: u8) -> ApiResult<FsThumb> {
    let (tw, th) = (img.width(), img.height());
    let (out, mime) = encode_image(img, FS_THUMB_MAX_BYTES)?;
    Ok(FsThumb {
        mime: mime.to_owned(),
        width: tw,
        height: th,
        orientation,
        embedded: false,
        data_hex: hex::encode(&out),
    })
}

pub(super) fn encode_image(
    img: &image::DynamicImage,
    max_bytes: usize,
) -> ApiResult<(Vec<u8>, &'static str)> {
    let transparent = img.color().has_alpha() && img.to_rgba8().pixels().any(|p| p.0[3] != 255);

    let mut out = Vec::with_capacity(64 * 1024);
    let mime = if transparent {
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .map_err(|e| ApiError::internal("缩略图编码失败").with_detail(e.to_string()))?;
        "image/png"
    } else {
        // 转成 RGB8 再编：JPEG 编码器不接受带 alpha 的输入，
        // 而上面的分支已经把真有透明的挑走了。
        let rgb = img.to_rgb8();
        let mut enc =
            image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut out), JPEG_QUALITY);
        enc.encode_image(&rgb)
            .map_err(|e| ApiError::internal("缩略图编码失败").with_detail(e.to_string()))?;
        "image/jpeg"
    };

    if out.len() > max_bytes {
        // 走到这里说明实现出了岔子（缩放没生效之类），如实报错。
        return Err(ApiError::internal(format!(
            "图片编出来 {} 字节，超过上限 {max_bytes}",
            out.len()
        )));
    }
    Ok((out, mime))
}

// ===========================================================================
// EXIF 内嵌预览：只读整数，不碰像素
// ===========================================================================

/// 从 JPEG 的 EXIF（APP1）里取出内嵌的预览 JPEG，返回它在 `bytes` 里的切片。
///
/// 全程只做边界检查与整数读取：先定位 APP1 段，再按 TIFF 的 IFD 结构走到
/// IFD1（缩略图目录），取 `JPEGInterchangeFormat`(0x0201) 与
/// `JPEGInterchangeFormatLength`(0x0202) 两个标签。任何一步对不上就返回
/// `None`——这是常态（截图、导出图往往没有内嵌预览），不是错误。
fn embedded_jpeg(bytes: &[u8]) -> Option<&[u8]> {
    exif_scan(bytes).0
}

/// EXIF 方向标签（0x0112，在 IFD0 里）。取不到按 1（正立）。
///
/// 与内嵌预览同一次遍历取出，见 [`exif_scan`]。
fn exif_orientation(bytes: &[u8]) -> u8 {
    exif_scan(bytes).1
}

/// 走一遍 EXIF，同时取出「内嵌预览」与「方向」。
///
/// 两件事共用同一次 IFD 遍历：分开写会把这段边界检查复制两份，
/// 而这段正是最不该有第二个副本的代码。
fn exif_scan(bytes: &[u8]) -> (Option<&[u8]>, u8) {
    // 整段实现放在一个返回 Option 的闭包里：中途任何一步对不上都是常态
    // （没有 EXIF、没有缩略图目录），用 `?` 表达比层层 if 清楚。
    let scan = || -> Option<(Option<&[u8]>, u8)> {
        let app1 = find_app1(bytes)?;
        // APP1 的净荷以 "Exif\0\0" 开头，其后才是 TIFF 头。
        let tiff = app1.strip_prefix(b"Exif\0\0")?;

        let le = match tiff.get(..2)? {
            b"II" => true,
            b"MM" => false,
            _ => return None,
        };
        let u16_at = |off: usize| -> Option<u16> {
            let b = tiff.get(off..off.checked_add(2)?)?;
            Some(if le {
                u16::from_le_bytes([b[0], b[1]])
            } else {
                u16::from_be_bytes([b[0], b[1]])
            })
        };
        let u32_at = |off: usize| -> Option<u32> {
            let b = tiff.get(off..off.checked_add(4)?)?;
            Some(if le {
                u32::from_le_bytes([b[0], b[1], b[2], b[3]])
            } else {
                u32::from_be_bytes([b[0], b[1], b[2], b[3]])
            })
        };

        if u16_at(2)? != 42 {
            return None; // TIFF 魔数
        }
        let ifd0 = u32_at(4)? as usize;
        let count0 = u16_at(ifd0)? as usize;

        // IFD0 里找方向标签（0x0112）。SHORT 类型，值就在条目的第 8 字节起。
        let mut orientation = 1u8;
        for i in 0..count0 {
            let entry = ifd0.checked_add(2 + i * 12)?;
            if u16_at(entry)? == 0x0112
                && let Some(v) = u16_at(entry + 8)
                && (1..=8).contains(&v)
            {
                orientation = v as u8;
            }
        }

        // IFD0 末尾的「下一个 IFD 偏移」就是 IFD1（缩略图目录）。
        let ifd1 = u32_at(ifd0 + 2 + count0 * 12)? as usize;
        if ifd1 == 0 {
            return Some((None, orientation)); // 没有缩略图，但方向仍然有效
        }

        let count1 = u16_at(ifd1)? as usize;
        let (mut offset, mut length) = (None, None);
        for i in 0..count1 {
            let entry = ifd1.checked_add(2 + i * 12)?;
            match u16_at(entry)? {
                0x0201 => offset = Some(u32_at(entry + 8)? as usize),
                0x0202 => length = Some(u32_at(entry + 8)? as usize),
                _ => {}
            }
        }
        let thumb = match (offset, length) {
            (Some(o), Some(l)) if l > 0 => tiff
                .get(o..o.checked_add(l)?)
                // 必须真的是一张 JPEG：SOI 魔数对不上就不要。
                .filter(|t| t.starts_with(&[0xFF, 0xD8])),
            _ => None,
        };
        Some((thumb, orientation))
    };
    scan().unwrap_or((None, 1))
}

/// 定位 JPEG 里的 APP1 段，返回它的净荷。
///
/// 只走标记链（每段自带长度），不做任何扫描式猜测；遇到 SOS（图像数据开始）
/// 即停——此后是熵编码数据，里面的 `0xFF` 不是段标记。
fn find_app1(bytes: &[u8]) -> Option<&[u8]> {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            return None; // 标记链断了，不猜
        }
        let marker = bytes[i + 1];
        if marker == 0xDA {
            return None; // SOS：再往后没有元数据了
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if len < 2 {
            return None;
        }
        let payload = bytes.get(i + 4..i + 2 + len)?;
        if marker == 0xE1 {
            return Some(payload);
        }
        i += 2 + len;
    }
    None
}

/// 从 JPEG 的 SOF 段读出宽高。内嵌预览要报真实尺寸，不能瞎填。
fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            return None;
        }
        let marker = bytes[i + 1];
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        // SOF0..SOF15，排除 DHT(C4)、JPG(C8)、DAC(CC) 这三个非 SOF 的。
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let seg = bytes.get(i + 4..i + 2 + len)?;
            // SOF 净荷：精度(1) 高(2) 宽(2) …
            let h = u16::from_be_bytes([*seg.get(1)?, *seg.get(2)?]);
            let w = u16::from_be_bytes([*seg.get(3)?, *seg.get(4)?]);
            return Some((u32::from(w), u32::from(h)));
        }
        if marker == 0xDA {
            return None;
        }
        if len < 2 {
            return None;
        }
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn 源文件超限在读取前拒绝() {
        let dir = TempDir::new("source-limit");
        let path = dir.join("large.jpg");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_SOURCE_BYTES + 1)
            .unwrap();
        let error = thumb_blocking(&path, 128).unwrap_err();
        assert!(error.message.contains("文件过大"));
    }

    #[tokio::test]
    async fn 取消请求仍持有解码配额直到阻塞工作退出() {
        static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let (started, mut ready) = tokio::sync::mpsc::unbounded_channel();
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let barrier = barrier.clone();
            let started = started.clone();
            tasks.push(tokio::spawn(bounded_decode(&SLOTS, move || {
                started.send(()).unwrap();
                barrier.wait();
                Ok(())
            })));
        }
        for _ in 0..2 {
            ready.recv().await.unwrap();
        }
        let full = bounded_decode(&SLOTS, || -> ApiResult<()> {
            panic!("超额工作不应执行")
        })
        .await;
        for task in tasks {
            task.abort();
        }
        let still_full = SLOTS.available_permits() == 0;
        // 先释放工作再断言，失败时也不能把阻塞线程留在 barrier 上。
        barrier.wait();
        assert_eq!(
            full.unwrap_err().code,
            strixmaid_types::ErrorCode::Unavailable
        );
        assert!(still_full, "请求取消不能提前释放实际仍在解码的 permit");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while SLOTS.available_permits() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn 解码_panic_变成错误且释放配额_下次请求仍可执行() {
        static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        let bad = bounded_decode(&SLOTS, || -> ApiResult<()> { panic!("注入解码故障") }).await;
        assert!(bad.is_err());
        assert_eq!(bounded_decode(&SLOTS, || Ok(42)).await.unwrap(), 42);
    }
    use super::*;
    use image::{Rgb, RgbImage, Rgba, RgbaImage};

    /// 房式临时目录（house style：不引 tempfile）。
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "strixmaid-thumb-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn join(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn 写一张大_jpeg(path: &Path, w: u32, h: u32) {
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = Rgb([(x % 256) as u8, (y % 256) as u8, 128]);
        }
        img.save_with_format(path, ImageFormat::Jpeg).unwrap();
    }

    #[test]
    fn 大图缩成小图且尺寸受限() {
        let dir = TempDir::new("big");
        let f = dir.join("big.jpg");
        写一张大_jpeg(&f, 1600, 1200);

        let t = thumb_blocking(&f, 256).expect("应当出得了缩略图");
        assert_eq!(t.mime, "image/jpeg");
        assert!(
            t.width.max(t.height) <= 256,
            "缩略图 {}×{} 超尺寸",
            t.width,
            t.height
        );
        // 4:3 的源，256 的长边 → 256×192。
        assert_eq!((t.width, t.height), (256, 192));
        assert!(!t.embedded, "这张没有内嵌预览，应当走解码路");

        let bytes = hex::decode(&t.data_hex).unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "不是合法 JPEG");
        // 真的小了：源图几百 KB，缩略图必须是几十 KB 这个量级。
        let src = std::fs::metadata(&f).unwrap().len();
        assert!(
            (bytes.len() as u64) < src / 4,
            "缩略图 {} 字节，源 {src} 字节——没有真的缩",
            bytes.len()
        );
        assert!(bytes.len() <= FS_THUMB_MAX_BYTES);
    }

    #[test]
    fn 带透明的_png_编成_png_不丢_alpha() {
        let dir = TempDir::new("alpha");
        let f = dir.join("a.png");
        let mut img = RgbaImage::new(300, 300);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = Rgba([255, 0, 0, if (x + y) % 2 == 0 { 0 } else { 255 }]);
        }
        img.save_with_format(&f, ImageFormat::Png).unwrap();

        let t = thumb_blocking(&f, 128).unwrap();
        assert_eq!(t.mime, "image/png", "有 alpha 的图编成 JPEG 会丢透明");
        let bytes = hex::decode(&t.data_hex).unwrap();
        assert_eq!(&bytes[..4], &[0x89, b'P', b'N', b'G']);
        let back = image::load_from_memory(&bytes).unwrap();
        assert!(back.color().has_alpha(), "alpha 通道没保住");
    }

    #[test]
    fn 小图不会被放大() {
        let dir = TempDir::new("small");
        let f = dir.join("s.png");
        RgbImage::new(32, 24)
            .save_with_format(&f, ImageFormat::Png)
            .unwrap();
        let t = thumb_blocking(&f, 256).unwrap();
        assert_eq!((t.width, t.height), (32, 24), "缩略图不该放大源图");
    }

    #[test]
    fn 内嵌预览走不解码的那条路() {
        // 造一张带 EXIF 缩略图的 JPEG：主图 + APP1（TIFF/IFD0/IFD1 + 内嵌小图）。
        let dir = TempDir::new("exif");
        let small = {
            let mut buf = Vec::new();
            let img = RgbImage::from_fn(160, 120, |x, _| Rgb([(x % 256) as u8, 7, 9]));
            img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Jpeg)
                .unwrap();
            buf
        };
        let main = {
            let mut buf = Vec::new();
            RgbImage::new(800, 600)
                .write_to(&mut Cursor::new(&mut buf), ImageFormat::Jpeg)
                .unwrap();
            buf
        };

        // TIFF（小端）：IFD0 空 → next=IFD1；IFD1 两个标签指向内嵌小图。
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend(b"II");
        tiff.extend(42u16.to_le_bytes());
        tiff.extend(8u32.to_le_bytes()); // IFD0 在偏移 8
        tiff.extend(0u16.to_le_bytes()); // IFD0：0 项
        let ifd1_off = 8 + 2 + 4; // IFD0 头 + next 指针
        tiff.extend((ifd1_off as u32).to_le_bytes()); // next = IFD1
        tiff.extend(2u16.to_le_bytes()); // IFD1：2 项
        let thumb_off = ifd1_off + 2 + 2 * 12 + 4;
        for (tag, value) in [(0x0201u16, thumb_off as u32), (0x0202, small.len() as u32)] {
            tiff.extend(tag.to_le_bytes());
            tiff.extend(4u16.to_le_bytes()); // type = LONG
            tiff.extend(1u32.to_le_bytes()); // count = 1
            tiff.extend(value.to_le_bytes());
        }
        tiff.extend(0u32.to_le_bytes()); // IFD1 的 next = 0
        assert_eq!(tiff.len(), thumb_off, "测试数据的偏移算错了");
        tiff.extend(&small);

        let mut payload = b"Exif\0\0".to_vec();
        payload.extend(&tiff);
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend(((payload.len() + 2) as u16).to_be_bytes());
        jpeg.extend(&payload);
        jpeg.extend(&main[2..]); // 主图去掉自己的 SOI

        let f = dir.join("exif.jpg");
        std::fs::write(&f, &jpeg).unwrap();

        let t = thumb_blocking(&f, 256).unwrap();
        assert!(t.embedded, "应当走 EXIF 内嵌预览这条路（不解码）");
        assert_eq!((t.width, t.height), (160, 120));
        assert_eq!(
            hex::decode(&t.data_hex).unwrap(),
            small,
            "取出的应当是内嵌的那张"
        );
    }

    #[test]
    fn 坏文件报错而不是崩溃() {
        let dir = TempDir::new("bad");
        let f = dir.join("bad.jpg");
        std::fs::write(&f, [0xFF, 0xD8, 0x00, 0x01, 0x02, 0x03]).unwrap();
        let err = thumb_blocking(&f, 256).expect_err("坏图应当报错");
        assert_eq!(err.code, strixmaid_types::ErrorCode::InvalidRequest);
    }

    #[test]
    fn 不支持的扩展名直接拒绝() {
        let dir = TempDir::new("ext");
        for bad in ["x.txt", "x.pdf", "x.svg", "x.mp4", "x"] {
            let f = dir.join(bad);
            std::fs::write(&f, b"whatever").unwrap();
            // 认不出的不喂进解码器碰运气。
            assert!(!supported(&f), "{bad} 不该被当成图片");
            assert!(thumb_blocking(&f, 256).is_err(), "{bad} 应当报错");
        }
        for ok in ["a.jpg", "a.JPEG", "a.png", "a.gif", "a.webp", "a.bmp"] {
            assert!(supported(Path::new(ok)), "{ok} 应当支持");
        }
    }

    #[test]
    fn heic_只在_macos_且有窗口服务器时支持() {
        // HEIC 的图像项是 HEVC 编码的、容器里也没有可捡的 JPEG 预览，
        // 所以它是唯一一个借系统解码器的格式，见 `system_decodable`。
        let heic = Path::new("a.heic");
        assert!(!rust_decodable(heic), "纯 Rust 解不了 HEIC");
        #[cfg(target_os = "macos")]
        assert_eq!(
            supported(heic),
            crate::platform::appkit::available(),
            "macOS 上支持与否应当跟着窗口服务器连接走"
        );
        #[cfg(not(target_os = "macos"))]
        assert!(!supported(heic), "macOS 之外没有这条路");
    }

    #[test]
    fn 系统解码器出得了_heic_缩略图() {
        if !system_decodable(Path::new("x.heic")) {
            eprintln!("本平台 / 本环境没有系统解码器这条路，跳过");
            return;
        }
        // 桌面图片是任何 macOS 上都有的 HEIC 样本；没有就跳过（精简系统）。
        let 样本 = ["/System/Library/Desktop Pictures/Mac Blue.heic"]
            .into_iter()
            .map(Path::new)
            .find(|p| p.is_file());
        let Some(src) = 样本 else {
            eprintln!("本机没有可用的 HEIC 样本，跳过");
            return;
        };
        let t = thumb_blocking(src, 256).expect("HEIC 应当出得了缩略图");
        assert!(
            t.width.max(t.height) <= 256,
            "{}×{} 超尺寸",
            t.width,
            t.height
        );
        assert!(!t.embedded, "HEIC 没有内嵌预览可捡，只能是解码出来的");
        assert_eq!(t.orientation, 1, "ImageIO 已应用方向，不该再让前端转一次");
        let bytes = hex::decode(&t.data_hex).unwrap();
        assert!(bytes.len() <= FS_THUMB_MAX_BYTES);
        // 解出来的必须是一张能再读回去的图，且不是一片空白。
        let back = image::load_from_memory(&bytes).expect("输出应当是合法图片");
        assert_eq!((back.width(), back.height()), (t.width, t.height));
        let rgb = back.to_rgb8();
        let 首像素 = rgb.pixels().next().unwrap().0;
        assert!(
            rgb.pixels().any(|p| p.0 != 首像素),
            "整张图一个颜色，多半是画失败了"
        );
    }

    #[test]
    fn 目录报_invalid_request() {
        let dir = TempDir::new("dir");
        let err = thumb_blocking(&dir.0, 256).expect_err("目录不该出缩略图");
        assert_eq!(err.code, strixmaid_types::ErrorCode::InvalidRequest);
    }

    #[test]
    fn app1_扫描不越界也不猜() {
        assert!(find_app1(b"").is_none());
        assert!(find_app1(&[0xFF, 0xD8]).is_none());
        // 标记链断了就停，不去扫描后面的 0xFFE1。
        assert!(find_app1(&[0xFF, 0xD8, 0x00, 0x01, 0xFF, 0xE1, 0x00, 0x08]).is_none());
        // 长度字段撒谎（超出缓冲）时不 panic。
        assert!(find_app1(&[0xFF, 0xD8, 0xFF, 0xE1, 0xFF, 0xFF, 0x01]).is_none());
        assert!(
            embedded_jpeg(&[0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x08, b'E', b'x', b'i', b'f']).is_none()
        );
    }
}
