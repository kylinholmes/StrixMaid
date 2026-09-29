//! 有界的 1600px 预览档，与 256px 缩略图契约分开。
use super::thumb;
use std::{fs::File, io::Read, path::Path};
use strixmaid_types::{ApiError, ApiResult};

const MAX_PX: u32 = 1600;
const MAX_BYTES: usize = 16 * 1024 * 1024;

pub(super) struct Rendered {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

/// 调用者持有流名额及共享解码名额；包括系统 HEIC 解码在内，
/// 一律读取已打开句柄，不能按路径重新打开文件。
pub(super) fn render(file: File, path: &Path) -> ApiResult<Rendered> {
    if file.metadata().map_err(|e| super::io_err(path, &e))?.len() > thumb::MAX_SOURCE_BYTES {
        return Err(ApiError::invalid_request("图片源文件超过读取上限"));
    }
    let mut bytes = Vec::new();
    file.take(thumb::MAX_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| super::io_err(path, &e))?;
    if bytes.len() as u64 > thumb::MAX_SOURCE_BYTES {
        return Err(ApiError::invalid_request("图片源文件超过读取上限"));
    }
    let img = if thumb::rust_decodable(path) {
        // 从主图解码，避免拉伸常见的 160px EXIF 内嵌缩略图。
        // 将 JPEG/PNG/WebP 的方向标签物理应用到缩小后的像素。
        thumb::decode_scaled(&bytes, MAX_PX, true)?
    } else {
        system_decode(&bytes)?
    };
    let (bytes, mime) = thumb::encode_image(&img, MAX_BYTES)?;
    Ok(Rendered { bytes, mime })
}

#[cfg(not(target_os = "macos"))]
fn system_decode(_: &[u8]) -> ApiResult<image::DynamicImage> {
    Err(ApiError::invalid_request("本平台不能解码此图片"))
}

#[cfg(target_os = "macos")]
fn system_decode(bytes: &[u8]) -> ApiResult<image::DynamicImage> {
    native::decode(bytes).ok_or_else(|| ApiError::invalid_request("系统图片解码失败或超过解码限额"))
}

/// 复用已有 HEIC 路径背后的系统解码能力，通过 CFData 传入句柄读出的字节。
/// ImageIO 生成尺寸受限且应用了 EXIF 方向的图片；先检查源像素尺寸，
/// 遵守与缩略图共享的像素数及解码内存上限。
#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::{ffi::c_void, ptr};
    type Ref = *const c_void;
    #[repr(C)]
    struct Point {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    struct Size {
        w: f64,
        h: f64,
    }
    #[repr(C)]
    struct Rect {
        origin: Point,
        size: Size,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: Ref);
        fn CFDataCreate(allocator: Ref, bytes: *const u8, len: isize) -> Ref;
        fn CFDictionaryCreate(
            allocator: Ref,
            keys: *const Ref,
            values: *const Ref,
            count: isize,
            key_callbacks: Ref,
            value_callbacks: Ref,
        ) -> Ref;
        fn CFDictionaryGetValue(dict: Ref, key: Ref) -> Ref;
        fn CFNumberCreate(allocator: Ref, kind: isize, value: *const c_void) -> Ref;
        fn CFNumberGetValue(number: Ref, kind: isize, value: *mut c_void) -> u8;
        fn CFGetTypeID(value: Ref) -> usize;
        fn CFNumberGetTypeID() -> usize;
        static kCFBooleanTrue: Ref;
        static kCFBooleanFalse: Ref;
    }
    #[link(name = "ImageIO", kind = "framework")]
    unsafe extern "C" {
        fn CGImageSourceCreateWithData(data: Ref, options: Ref) -> Ref;
        fn CGImageSourceCopyPropertiesAtIndex(source: Ref, index: usize, options: Ref) -> Ref;
        fn CGImageSourceCreateThumbnailAtIndex(source: Ref, index: usize, options: Ref) -> Ref;
        static kCGImagePropertyPixelWidth: Ref;
        static kCGImagePropertyPixelHeight: Ref;
        static kCGImageSourceShouldCache: Ref;
        static kCGImageSourceCreateThumbnailFromImageAlways: Ref;
        static kCGImageSourceCreateThumbnailWithTransform: Ref;
        static kCGImageSourceThumbnailMaxPixelSize: Ref;
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGImageGetWidth(image: Ref) -> usize;
        fn CGImageGetHeight(image: Ref) -> usize;
        fn CGColorSpaceCreateDeviceRGB() -> Ref;
        fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits: usize,
            row_bytes: usize,
            space: Ref,
            bitmap_info: u32,
        ) -> Ref;
        fn CGContextDrawImage(context: Ref, rect: Rect, image: Ref);
    }
    struct Owned(Ref);
    impl Owned {
        fn new(value: Ref) -> Option<Self> {
            (!value.is_null()).then_some(Self(value))
        }
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: 此处独占非空且引用计数 +1 的 CF/CG 对象。
            unsafe { CFRelease(self.0) }
        }
    }

    pub(super) fn decode(bytes: &[u8]) -> Option<image::DynamicImage> {
        // SAFETY: CFData 复制有界切片，所有 +1 对象均由 RAII 释放。
        // 无回调字典借用的键值在调用期间存活；数值转换前检查类型，
        // 调用绘制接口前检查位图尺寸及底层缓冲长度。
        unsafe {
            let data = Owned::new(CFDataCreate(
                ptr::null(),
                bytes.as_ptr(),
                bytes.len().try_into().ok()?,
            ))?;
            let keys = [kCGImageSourceShouldCache];
            let values = [kCFBooleanFalse];
            let options = Owned::new(CFDictionaryCreate(
                ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                ptr::null(),
                ptr::null(),
            ))?;
            let source = Owned::new(CGImageSourceCreateWithData(data.0, options.0))?;
            let props = Owned::new(CGImageSourceCopyPropertiesAtIndex(source.0, 0, options.0))?;
            let mut dimensions = [0i64; 2];
            for (key, out) in [kCGImagePropertyPixelWidth, kCGImagePropertyPixelHeight]
                .into_iter()
                .zip(&mut dimensions)
            {
                let n = CFDictionaryGetValue(props.0, key);
                if n.is_null()
                    || CFGetTypeID(n) != CFNumberGetTypeID()
                    || CFNumberGetValue(n, 4, (out as *mut i64).cast()) == 0
                {
                    return None;
                }
            }
            let [w, h] = dimensions;
            let pixels = u64::try_from(w).ok()?.checked_mul(u64::try_from(h).ok()?)?;
            if w <= 0
                || h <= 0
                || pixels > thumb::MAX_PIXELS
                || pixels.checked_mul(4)? > thumb::MAX_DECODE_BYTES
            {
                return None;
            }
            let max = i64::from(MAX_PX);
            let number = Owned::new(CFNumberCreate(ptr::null(), 4, (&max as *const i64).cast()))?;
            let keys = [
                kCGImageSourceCreateThumbnailFromImageAlways,
                kCGImageSourceCreateThumbnailWithTransform,
                kCGImageSourceThumbnailMaxPixelSize,
                kCGImageSourceShouldCache,
            ];
            let values = [kCFBooleanTrue, kCFBooleanTrue, number.0, kCFBooleanFalse];
            let options = Owned::new(CFDictionaryCreate(
                ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                4,
                ptr::null(),
                ptr::null(),
            ))?;
            let image = Owned::new(CGImageSourceCreateThumbnailAtIndex(source.0, 0, options.0))?;
            let (w, h) = (CGImageGetWidth(image.0), CGImageGetHeight(image.0));
            if w == 0 || h == 0 || w > MAX_PX as usize || h > MAX_PX as usize {
                return None;
            }
            let mut rgba = vec![0u8; w.checked_mul(h)?.checked_mul(4)?];
            let space = Owned::new(CGColorSpaceCreateDeviceRGB())?;
            // 大端字节序 + 末通道预乘 alpha，输出内存排列为 RGBA。
            let context = Owned::new(CGBitmapContextCreate(
                rgba.as_mut_ptr().cast(),
                w,
                h,
                8,
                w * 4,
                space.0,
                (4 << 12) | 1,
            ))?;
            CGContextDrawImage(
                context.0,
                Rect {
                    origin: Point { x: 0.0, y: 0.0 },
                    size: Size {
                        w: w as f64,
                        h: h as f64,
                    },
                },
                image.0,
            );
            drop(context);
            for pixel in rgba.as_chunks_mut::<4>().0 {
                let a = u32::from(pixel[3]);
                if a > 0 && a < 255 {
                    for c in &mut pixel[..3] {
                        *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
                    }
                }
            }
            Some(image::DynamicImage::ImageRgba8(image::RgbaImage::from_raw(
                w as u32, h as u32, rgba,
            )?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageEncoder, ImageFormat};
    use std::io::Cursor;

    #[test]
    fn exif八种方向已物理转正_输出无方向元数据且保留透明() {
        let pixels: Vec<u8> = (1..=6).flat_map(|n| [n * 30, 0, 0, 128]).collect();
        let expected = [
            vec![1, 2, 3, 4, 5, 6],
            vec![3, 2, 1, 6, 5, 4],
            vec![6, 5, 4, 3, 2, 1],
            vec![4, 5, 6, 1, 2, 3],
            vec![1, 4, 2, 5, 3, 6],
            vec![4, 1, 5, 2, 6, 3],
            vec![6, 3, 5, 2, 4, 1],
            vec![3, 6, 2, 5, 1, 4],
        ];
        for orientation in 1..=8u16 {
            let mut tiff = b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
            tiff.extend(orientation.to_le_bytes());
            tiff.extend([0; 6]);
            let mut encoded = Vec::new();
            let mut encoder = image::codecs::png::PngEncoder::new(&mut encoded);
            encoder.set_exif_metadata(tiff).unwrap();
            encoder
                .write_image(&pixels, 3, 2, image::ExtendedColorType::Rgba8)
                .unwrap();
            let oriented = thumb::decode_scaled(&encoded, MAX_PX, true).unwrap();
            let (bytes, mime) = thumb::encode_image(&oriented, MAX_BYTES).unwrap();
            assert_eq!(mime, "image/png");
            let decoded = image::load_from_memory(&bytes).unwrap().into_rgba8();
            let dimensions = if orientation <= 4 { (3, 2) } else { (2, 3) };
            assert_eq!(decoded.dimensions(), dimensions);
            assert_eq!(
                decoded.pixels().map(|p| p[0] / 30).collect::<Vec<_>>(),
                expected[orientation as usize - 1]
            );
            assert!(decoded.pixels().all(|p| p[3] == 128));
            use image::ImageDecoder;
            let mut decoder = image::ImageReader::new(Cursor::new(&bytes))
                .with_guessed_format()
                .unwrap()
                .into_decoder()
                .unwrap();
            assert_eq!(
                decoder.orientation().unwrap(),
                image::metadata::Orientation::NoTransforms
            );
        }
    }

    #[test]
    fn 超限图片在解码前拒绝_小图不放大_缩略图仍夹256() {
        // BMP 只放头，不放像素：若没有尺寸闸，会进入读像素而报 EOF。
        let mut bmp = vec![0; 54];
        bmp[..2].copy_from_slice(b"BM");
        bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        bmp[18..22].copy_from_slice(&10000i32.to_le_bytes());
        bmp[22..26].copy_from_slice(&10000i32.to_le_bytes());
        bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
        bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
        let err = thumb::decode_scaled(&bmp, MAX_PX, true).unwrap_err();
        assert!(err.message.contains("像素过多"), "{err:?}");

        let mut png = Cursor::new(Vec::new());
        image::RgbImage::new(12, 8)
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        let small = thumb::decode_scaled(png.get_ref(), MAX_PX, true).unwrap();
        assert_eq!((small.width(), small.height()), (12, 8));
        assert_eq!(strixmaid_types::rpc::FS_THUMB_MAX_PX, 256);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn 系统heic由已打开文件解码为有界预览() {
        let path = Path::new("/System/Library/Desktop Pictures/Mac Blue.heic");
        if !path.is_file() {
            eprintln!("本机缺少系统 HEIC 样本，跳过");
            return;
        }
        let rendered = render(File::open(path).unwrap(), path).unwrap();
        assert!(rendered.bytes.len() <= MAX_BYTES);
        let image = image::load_from_memory(&rendered.bytes).unwrap();
        assert!(image.width().max(image.height()) <= MAX_PX);
        assert!(image.width().max(image.height()) > strixmaid_types::rpc::FS_THUMB_MAX_PX);
    }
}
