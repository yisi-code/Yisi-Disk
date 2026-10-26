//! 预览恢复出的文件：开头的字节供十六进制视图；JPEG、PNG 解码后缩小，重新编码成 PNG。
//! 恢复出的文件可能是残缺或伪造的，解码只在引擎进程里进行，并限制尺寸与内存。

use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::Path;

use image::{ImageFormat, ImageReader, Limits};
use yisi_engine::protocol::{FilePreview, ImagePreview, PREVIEW_HEAD_BYTES, THUMBNAIL_EDGE};

/// 超过这个大小的图片不解码，只给十六进制视图。
const MAX_IMAGE_FILE: u64 = 64 << 20;
/// 解码时允许分配的内存上限，挡住尺寸字段离谱的图片。
const MAX_DECODE_ALLOC: u64 = 256 << 20;
const MAX_IMAGE_EDGE: u32 = 16_384;

pub fn preview(path: &Path) -> io::Result<FilePreview> {
    let file = File::open(path)?;
    let length = file.metadata()?.len();
    preview_from(file, length)
}

/// `length` 是文件总长度，`file` 从文件开头读起。
fn preview_from(mut file: impl Read, length: u64) -> io::Result<FilePreview> {
    let mut head = Vec::with_capacity(PREVIEW_HEAD_BYTES);
    (&mut file)
        .take(PREVIEW_HEAD_BYTES as u64)
        .read_to_end(&mut head)?;

    let (image, image_error) = match image_format(&head) {
        Some(_) if length > MAX_IMAGE_FILE => (
            None,
            Some(format!("图片超过 {} MiB，不解码", MAX_IMAGE_FILE >> 20)),
        ),
        Some(format) => {
            let mut data = head.clone();
            file.read_to_end(&mut data)?;
            match thumbnail(&data, format) {
                Ok(image) => (Some(image), None),
                Err(error) => (None, Some(error.to_string())),
            }
        }
        None => (None, None),
    };
    Ok(FilePreview {
        length,
        head: base64(&head),
        image,
        image_error,
    })
}

/// 按内容判断，只认 JPEG 和 PNG。
fn image_format(head: &[u8]) -> Option<ImageFormat> {
    match image::guess_format(head) {
        Ok(format @ (ImageFormat::Jpeg | ImageFormat::Png)) => Some(format),
        _ => None,
    }
}

fn thumbnail(data: &[u8], format: ImageFormat) -> image::ImageResult<ImagePreview> {
    let mut reader = ImageReader::with_format(Cursor::new(data), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_EDGE);
    limits.max_image_height = Some(MAX_IMAGE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let decoded = reader.decode()?;
    let (width, height) = (decoded.width(), decoded.height());
    // 小图原样重新编码，不放大
    let small = if width > THUMBNAIL_EDGE || height > THUMBNAIL_EDGE {
        decoded.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE)
    } else {
        decoded
    };
    let mut png = Vec::new();
    small.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    Ok(ImagePreview {
        width,
        height,
        thumbnail: base64(&png),
    })
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for group in data.chunks(3) {
        let bytes = [
            group.first().copied().unwrap_or(0),
            group.get(1).copied().unwrap_or(0),
            group.get(2).copied().unwrap_or(0),
        ];
        let bits = u32::from(bytes[0]) << 16 | u32::from(bytes[1]) << 8 | u32::from(bytes[2]);
        for index in 0..4 {
            if index <= group.len() {
                let sextet = (bits >> (18 - 6 * index)) & 0x3F;
                out.push(char::from(ALPHABET[sextet as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use image::{Rgb, RgbImage};
    use yisi_formats::samples;

    use super::*;

    fn preview_bytes(data: &[u8]) -> FilePreview {
        preview_from(data, data.len() as u64).unwrap()
    }

    fn decode_base64(text: &str) -> Vec<u8> {
        let value = |ch: u8| match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            _ => 63,
        };
        let mut out = Vec::new();
        for group in text.as_bytes().chunks(4) {
            let pad = group.iter().filter(|&&ch| ch == b'=').count();
            let bits = group.iter().fold(0u32, |bits, &ch| {
                bits << 6 | u32::from(if ch == b'=' { 0 } else { value(ch) })
            });
            let bytes = [(bits >> 16) as u8, (bits >> 8) as u8, bits as u8];
            out.extend_from_slice(&bytes[..3 - pad]);
        }
        out
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xFB, 0xFF]), "+/8=");
    }

    #[test]
    fn large_png_gets_a_shrunk_thumbnail_with_original_size() {
        let picture = RgbImage::from_fn(1200, 300, |x, _| Rgb([(x % 256) as u8, 0, 0]));
        let mut png = Vec::new();
        picture
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();

        let preview = preview_bytes(&png);
        assert_eq!(preview.length, png.len() as u64);
        assert_eq!(
            decode_base64(&preview.head),
            png[..PREVIEW_HEAD_BYTES.min(png.len())]
        );
        let image = preview.image.unwrap();
        assert_eq!((image.width, image.height), (1200, 300));
        let thumbnail = image::load_from_memory(&decode_base64(&image.thumbnail)).unwrap();
        assert_eq!((thumbnail.width(), thumbnail.height()), (512, 128));
        assert!(preview.image_error.is_none());
    }

    #[test]
    fn broken_image_reports_why_and_still_gives_the_head() {
        // 文件头是 PNG，IHDR 之后就断了
        let mut png = Vec::new();
        RgbImage::new(4, 4)
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        png.truncate(40);

        let preview = preview_bytes(&png);
        assert!(preview.image.is_none());
        assert!(preview.image_error.is_some());
        assert_eq!(decode_base64(&preview.head), png);
    }

    #[test]
    fn other_formats_only_get_the_head() {
        let zip = samples::zip(&[("a.txt", 9000, true)], b"");
        let preview = preview_bytes(&zip);
        assert_eq!(preview.length, zip.len() as u64);
        assert_eq!(
            decode_base64(&preview.head),
            zip[..PREVIEW_HEAD_BYTES.min(zip.len())]
        );
        assert!(preview.image.is_none() && preview.image_error.is_none());
    }
}
