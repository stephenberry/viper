//! Image detection and preparation for model input.

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine;
use image::{DynamicImage, ImageFormat, imageops::FilterType};

use crate::message::ContentBlock;

/// Longest edge, in pixels, of images sent to the model.
const MAX_DIMENSION: u32 = 2000;
/// Maximum base64 payload per image.
const MAX_ENCODED_BYTES: usize = 4_500_000;
const JPEG_QUALITY: u8 = 80;

/// Detect a supported image MIME type from the file's leading bytes.
pub fn detect_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Whether `path` names a file whose content is a supported image.
pub fn is_image_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let mut header = [0u8; 16];
    let Ok(n) = file.read(&mut header) else { return false };
    detect_mime(&header[..n]).is_some()
}

pub struct PreparedImage {
    pub block: ContentBlock,
    /// Notes for the model, e.g. that the image was resized.
    pub notes: Vec<String>,
}

fn encode(image: &DynamicImage, format: ImageFormat) -> Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    match format {
        ImageFormat::Jpeg => {
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
            image.to_rgb8().write_with_encoder(encoder)?;
        }
        _ => image.write_to(&mut out, format)?,
    }
    Ok(out.into_inner())
}

fn base64_len(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

/// Convert raw image bytes into a content block the model accepts: PNG/JPEG/WebP within the size
/// limits. Oversized images are downscaled and re-encoded.
pub fn prepare(bytes: &[u8]) -> Result<PreparedImage> {
    let mime = detect_mime(bytes).context("unsupported image format")?;
    let engine = base64::engine::general_purpose::STANDARD;
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let dims = reader.into_dimensions().ok();
    let fits = dims.is_some_and(|(w, h)| w <= MAX_DIMENSION && h <= MAX_DIMENSION);
    if fits && base64_len(bytes.len()) <= MAX_ENCODED_BYTES {
        return Ok(PreparedImage {
            block: ContentBlock::Image { data: engine.encode(bytes), mime_type: mime.to_string() },
            notes: Vec::new(),
        });
    }

    let image = image::load_from_memory(bytes).context("failed to decode image")?;
    let (orig_w, orig_h) = (image.width(), image.height());
    let mut resized = if orig_w > MAX_DIMENSION || orig_h > MAX_DIMENSION {
        image.resize(MAX_DIMENSION, MAX_DIMENSION, FilterType::Lanczos3)
    } else {
        image
    };

    // PNG keeps text crisp; fall back to JPEG and smaller sizes until the payload fits.
    let mut format = if mime == "image/jpeg" { ImageFormat::Jpeg } else { ImageFormat::Png };
    let mut encoded = encode(&resized, format)?;
    if base64_len(encoded.len()) > MAX_ENCODED_BYTES && format == ImageFormat::Png {
        format = ImageFormat::Jpeg;
        encoded = encode(&resized, format)?;
    }
    while base64_len(encoded.len()) > MAX_ENCODED_BYTES && resized.width() > 256 {
        let (w, h) = (resized.width() * 3 / 4, resized.height() * 3 / 4);
        resized = resized.resize(w, h, FilterType::Lanczos3);
        encoded = encode(&resized, format)?;
    }

    let mut notes = Vec::new();
    if (resized.width(), resized.height()) != (orig_w, orig_h) {
        notes.push(format!(
            "[Image resized from {orig_w}x{orig_h} to {}x{}. Coordinates in the image are scaled by {:.3}.]",
            resized.width(),
            resized.height(),
            orig_w as f64 / resized.width() as f64
        ));
    }
    let mime_type = if format == ImageFormat::Jpeg { "image/jpeg" } else { "image/png" };
    Ok(PreparedImage {
        block: ContentBlock::Image { data: engine.encode(&encoded), mime_type: mime_type.into() },
        notes,
    })
}

/// Read and prepare an image file.
pub fn load_file(path: &Path) -> Result<PreparedImage> {
    let bytes = std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    prepare(&bytes)
}

/// Encode an RGBA buffer (e.g. from the clipboard) as an image block.
pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Result<PreparedImage> {
    let buffer = image::RgbaImage::from_raw(width, height, rgba).context("invalid clipboard image buffer")?;
    let png = encode(&DynamicImage::ImageRgba8(buffer), ImageFormat::Png)?;
    prepare(&png)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::new(width, height));
        encode(&image, ImageFormat::Png).unwrap()
    }

    #[test]
    fn detects_formats() {
        assert_eq!(detect_mime(&png(1, 1)), Some("image/png"));
        assert_eq!(detect_mime(b"GIF89a...."), None);
        assert_eq!(detect_mime(b"hello"), None);
    }

    #[test]
    fn small_images_pass_through() {
        let bytes = png(10, 10);
        let prepared = prepare(&bytes).unwrap();
        assert!(prepared.notes.is_empty());
        let ContentBlock::Image { mime_type, .. } = prepared.block else { panic!() };
        assert_eq!(mime_type, "image/png");
    }

    #[test]
    fn large_images_are_resized() {
        let prepared = prepare(&png(3000, 1000)).unwrap();
        assert_eq!(prepared.notes.len(), 1);
        assert!(prepared.notes[0].contains("3000x1000 to 2000x667"));
    }
}
