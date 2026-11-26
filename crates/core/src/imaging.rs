//! Turning a file on disk into an image a provider will accept.
//!
//! `read` hands every decodable image to the model, but providers cap the pixels
//! they are willing to decode and charge by them. An oversized screenshot is
//! therefore resized rather than refused: [`prepare`] keeps the file as it is
//! when its longest edge already fits `image.maxEdge`, and otherwise scales it
//! down to that edge - JPEG stays JPEG, everything else is re-encoded as PNG.

use std::io::Cursor;

use image::{DynamicImage, ImageFormat};

/// An image ready to be attached: encoded bytes plus the MIME type that
/// describes them.
#[derive(Debug)]
pub struct Prepared {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    /// Longest edge of the file that was read, before any downscaling.
    pub original_edge: u32,
    pub scaled: bool,
}

/// Why a file was not handed over as an image.
#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    /// Not an image this build can decode, or not an image at all. The caller
    /// treats the file as ordinary data.
    NotAnImage,
    /// Decoding would cost more memory than the harness is willing to spend on
    /// one screenshot: a 100 megapixel PNG expands to 400 MB of RGBA before it
    /// can be shrunk. The caller reports the real dimensions.
    TooManyPixels { width: u32, height: u32 },
}

/// Beyond this many pixels a file is refused instead of decoded: the expanded
/// RGBA buffer would be 4 bytes per pixel, and shrinking is only possible after
/// the whole image exists in memory.
pub const MAX_PIXELS: u64 = 40_000_000;

/// Decode `bytes` and fit it within `max_edge` pixels on its longest side.
pub fn prepare(bytes: &[u8], max_edge: u32) -> Result<Prepared, Rejected> {
    let format = image::guess_format(bytes).map_err(|_| Rejected::NotAnImage)?;
    let (width, height) = image::ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|_| Rejected::NotAnImage)?;
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(Rejected::TooManyPixels { width, height });
    }
    let image =
        image::load_from_memory_with_format(bytes, format).map_err(|_| Rejected::NotAnImage)?;
    let (width, height) = (image.width(), image.height());
    let original_edge = width.max(height);
    if original_edge <= max_edge {
        if let Some(mime) = passthrough_mime(format) {
            return Ok(Prepared {
                bytes: bytes.to_vec(),
                mime,
                width,
                height,
                original_edge,
                scaled: false,
            });
        }
    }
    let (bytes, mime) = resize(&image, format, max_edge);
    let (width, height) = decoded_size(&bytes).unwrap_or((width, height));
    Ok(Prepared {
        bytes,
        mime,
        width,
        height,
        original_edge,
        scaled: true,
    })
}

/// Formats every provider accepts verbatim. Anything else is re-encoded to PNG
/// so a TIFF or a DDS does not reach the API as an unlabelled blob.
fn passthrough_mime(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::Gif => Some("image/gif"),
        ImageFormat::WebP => Some("image/webp"),
        _ => None,
    }
}

fn keeps_alpha(image: &DynamicImage) -> bool {
    image.color().has_alpha()
}

/// Scale the longest edge down to `max_edge`, keeping the aspect ratio. One
/// pass is enough: the target is a pixel count, not an encoded size, and a
/// provider's per-image byte cap is comfortably above what a 1568 px image
/// weighs in either format.
fn resize(image: &DynamicImage, format: ImageFormat, max_edge: u32) -> (Vec<u8>, &'static str) {
    let jpeg = format == ImageFormat::Jpeg;
    let (width, height) = (image.width(), image.height());
    let scale = f64::from(max_edge) / f64::from(width.max(height));
    let target_width = ((f64::from(width) * scale).round() as u32).max(1);
    let target_height = ((f64::from(height) * scale).round() as u32).max(1);
    let resized = image.resize_exact(
        target_width,
        target_height,
        image::imageops::FilterType::Lanczos3,
    );
    let mime = if jpeg { "image/jpeg" } else { "image/png" };
    (encode(&resized, jpeg), mime)
}

fn encode(image: &DynamicImage, jpeg: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if jpeg {
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
        // JPEG has no alpha: flatten rather than fail on a transparent PNG.
        let _ = encoder.encode_image(&to_rgb(image));
    } else {
        let _ = image.write_to(&mut Cursor::new(&mut out), ImageFormat::Png);
    }
    out
}

fn to_rgb(image: &DynamicImage) -> DynamicImage {
    if keeps_alpha(image) {
        let mut flat = image.to_rgba8();
        for pixel in flat.pixels_mut() {
            let [r, g, b, a] = pixel.0;
            let alpha = a as u32;
            pixel.0 = [
                ((r as u32 * alpha + 255 * (255 - alpha)) / 255) as u8,
                ((g as u32 * alpha + 255 * (255 - alpha)) / 255) as u8,
                ((b as u32 * alpha + 255 * (255 - alpha)) / 255) as u8,
                255,
            ];
        }
        DynamicImage::ImageRgba8(flat)
    } else {
        image.clone()
    }
}

fn decoded_size(bytes: &[u8]) -> Option<(u32, u32)> {
    image::load_from_memory(bytes)
        .ok()
        .map(|image| (image.width(), image.height()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    /// A noisy image compresses badly, so the byte limit is what drives the
    /// downscale rather than the pixel count.
    fn noisy(size: u32) -> Vec<u8> {
        let mut image = RgbaImage::new(size, size);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgba([(x % 251) as u8, (y % 241) as u8, ((x * y) % 239) as u8, 255]);
        }
        let mut out = Vec::new();
        DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn an_image_that_already_fits_is_passed_through() {
        let bytes = noisy(64);
        let prepared = prepare(&bytes, 1568).unwrap();
        assert_eq!(prepared.mime, "image/png");
        assert_eq!(prepared.bytes, bytes);
        assert!(!prepared.scaled);
        assert_eq!((prepared.width, prepared.height), (64, 64));
    }

    #[test]
    fn an_image_with_a_long_edge_over_the_limit_is_shrunk() {
        let bytes = noisy(512);
        let prepared = prepare(&bytes, 128).unwrap();
        assert!(prepared.scaled);
        assert_eq!(prepared.original_edge, 512);
        assert_eq!((prepared.width, prepared.height), (128, 128));
        // The result is still a real image of the size it claims.
        assert_eq!(
            decoded_size(&prepared.bytes),
            Some((prepared.width, prepared.height))
        );
    }

    #[test]
    fn non_provider_formats_are_re_encoded_as_png() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(8, 8, Rgba([1, 2, 3, 255])));
        let mut bmp = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bmp), ImageFormat::Bmp)
            .unwrap();
        let prepared = prepare(&bmp, 1568).unwrap();
        assert_eq!(prepared.mime, "image/png");
        assert_eq!(&prepared.bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn jpeg_stays_jpeg_when_it_has_to_shrink() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(400, 300, Rgba([9, 9, 9, 255])));
        let mut jpeg = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95);
        encoder.encode_image(&image).unwrap();
        let prepared = prepare(&jpeg, 100).unwrap();
        assert!(prepared.scaled);
        assert_eq!(prepared.mime, "image/jpeg");
        // 400x300 keeps its ratio: the long edge becomes 100.
        assert_eq!((prepared.width, prepared.height), (100, 75));
    }

    /// A tall image keeps its shape too: only the long edge is capped.
    #[test]
    fn the_aspect_ratio_survives_the_downscale() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(300, 900, Rgba([4, 5, 6, 255])));
        let mut png = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        let prepared = prepare(&png, 300).unwrap();
        assert_eq!((prepared.width, prepared.height), (100, 300));
    }

    #[test]
    fn text_is_not_an_image() {
        assert_eq!(
            prepare(b"hello, world\n", 1568).unwrap_err(),
            Rejected::NotAnImage
        );
    }

    /// Dimensions come from the header and are checked before the pixels are
    /// decoded, so a file claiming more than the harness will spend on one
    /// image is refused up front. A PNM header is the cheapest way to claim
    /// 8000x8000 without paying for the pixels.
    #[test]
    fn an_absurd_pixel_count_is_refused_up_front() {
        let mut pnm = b"P5\n8000 8000\n255\n".to_vec();
        pnm.push(0);
        assert_eq!(
            prepare(&pnm, 1568).unwrap_err(),
            Rejected::TooManyPixels {
                width: 8000,
                height: 8000
            }
        );
    }
}
