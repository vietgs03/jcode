//! Clamp outbound image dimensions so vision requests stay within provider
//! per-image pixel limits.
//!
//! ## Why this exists
//!
//! Anthropic's Messages API rejects requests whose images exceed a per-image
//! pixel cap that depends on how many images the request carries:
//!
//! * requests with **more than 20 images**: each image must be `<= 2000px` on
//!   every edge,
//! * requests with **20 or fewer images**: each image must be `<= 8000px`.
//!
//! A single oversized screenshot is therefore fine in a small turn but makes the
//! whole request 400 once a session accumulates more than 20 images. The most
//! common trigger is **resuming a session**: every stored screenshot is replayed
//! into one request, instantly crossing the 20-image threshold and failing with
//!
//! ```text
//! messages.N.content.M.image.source.base64.data: At least one of the image
//! dimensions exceed max allowed size for many-image requests: 2000 pixels
//! ```
//!
//! Clamping every outbound image's longest edge to 2000px satisfies *both*
//! thresholds (2000 <= 2000 and 2000 <= 8000), keeps detail well within
//! Anthropic's own internal downscale target (~1568px long edge), and is a no-op
//! for the overwhelmingly common case of already-small images.
//!
//! ## Design
//!
//! * Fast path: parse width/height straight out of the encoded header (no full
//!   decode). If the image is already within bounds we hand the original payload
//!   back as a borrow, so the hot path allocates nothing.
//! * Slow path: fully decode, downscale preserving aspect ratio, and re-encode.
//!   Results are memoized in a small bounded cache so resuming a session (which
//!   rebuilds the same request many times) only pays the resize cost once.

use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::sync::OnceLock;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

/// Longest-edge cap that satisfies Anthropic's strictest (many-image) limit.
pub const ANTHROPIC_MANY_IMAGE_MAX_EDGE: u32 = 2000;

/// Environment override for the clamp edge, mostly for testing/diagnostics.
const MAX_EDGE_ENV: &str = "JCODE_IMAGE_MAX_EDGE";

/// JPEG quality used when re-encoding downscaled JPEG inputs.
const JPEG_REENCODE_QUALITY: u8 = 85;

/// Upper bound on memoized clamp results. Each entry stores a downscaled image
/// (<= ~2000px), so this is a few tens of MB worst case.
const CACHE_CAPACITY: usize = 64;

/// Resolve the effective max edge, honoring the env override when present.
pub fn effective_max_edge() -> u32 {
    std::env::var(MAX_EDGE_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(ANTHROPIC_MANY_IMAGE_MAX_EDGE)
}

/// Clamp a base64-encoded image so neither edge exceeds the configured limit.
///
/// Returns the `(media_type, base64_data)` that should actually be sent. When no
/// work is needed (already small, or the payload cannot be processed) the
/// original inputs are returned as borrows so the common path is allocation-free.
pub fn clamp_base64_image<'a>(
    media_type: &'a str,
    data: &'a str,
) -> (Cow<'a, str>, Cow<'a, str>) {
    clamp_base64_image_with_edge(media_type, data, effective_max_edge())
}

/// Like [`clamp_base64_image`] but with an explicit edge limit (used by tests).
pub fn clamp_base64_image_with_edge<'a>(
    media_type: &'a str,
    data: &'a str,
    max_edge: u32,
) -> (Cow<'a, str>, Cow<'a, str>) {
    if max_edge == 0 {
        return (Cow::Borrowed(media_type), Cow::Borrowed(data));
    }

    // Fast path: read dimensions from the encoded header without a full decode.
    // If we can confirm the image already fits, send it untouched.
    if let Some((w, h)) = dimensions_from_base64_header(data)
        && w <= max_edge
        && h <= max_edge
    {
        return (Cow::Borrowed(media_type), Cow::Borrowed(data));
    }

    // Memoize the (expensive) decode+resize+encode keyed by the raw payload so a
    // resumed session that rebuilds the request repeatedly only pays once.
    let key = CacheKey {
        hash: hash_str(data),
        len: data.len(),
        max_edge,
    };
    if let Some(hit) = cache_get(&key) {
        return (Cow::Owned(hit.0), Cow::Owned(hit.1));
    }

    match reencode_clamped(media_type, data, max_edge) {
        Some((mt, encoded)) => {
            cache_put(key, (mt.clone(), encoded.clone()));
            (Cow::Owned(mt), Cow::Owned(encoded))
        }
        None => {
            // Could not parse/decode (e.g. webp/gif without decode support, or a
            // dimension reader miss on an actually-fine image). Fall back to the
            // original payload rather than dropping the image.
            (Cow::Borrowed(media_type), Cow::Borrowed(data))
        }
    }
}

/// Decode, downscale, and re-encode an oversized image. Returns `None` when the
/// payload cannot be decoded or already fits (no change required).
fn reencode_clamped(media_type: &str, data: &str, max_edge: u32) -> Option<(String, String)> {
    let bytes = BASE64.decode(data.trim().as_bytes()).ok()?;

    let decoded = image::load_from_memory(&bytes).ok()?;
    let (w, h) = (decoded.width(), decoded.height());
    if w <= max_edge && h <= max_edge {
        // Header parser missed it but the real dimensions are fine; no work.
        return None;
    }

    // Preserve aspect ratio; `resize` fits within the bounding box.
    let resized = decoded.resize(max_edge, max_edge, image::imageops::FilterType::Lanczos3);

    let prefer_jpeg = media_type.eq_ignore_ascii_case("image/jpeg")
        || media_type.eq_ignore_ascii_case("image/jpg");

    let (out_bytes, out_media) = if prefer_jpeg {
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        // JPEG cannot carry alpha; flatten to RGB before encoding.
        let rgb = resized.to_rgb8();
        let encoder =
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, JPEG_REENCODE_QUALITY);
        image::ImageEncoder::write_image(
            encoder,
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .ok()?;
        (buf, "image/jpeg".to_string())
    } else {
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        resized
            .write_to(&mut cursor, image::ImageFormat::Png)
            .ok()?;
        (buf, "image/png".to_string())
    };

    jcode_logging::info(&format!(
        "image-clamp: downscaled {}x{} -> {}x{} ({} -> {}, {} bytes)",
        w,
        h,
        resized.width(),
        resized.height(),
        media_type,
        out_media,
        out_bytes.len(),
    ));

    Some((out_media, BASE64.encode(&out_bytes)))
}

/// Parse image dimensions directly from the (possibly very large) base64 string
/// by decoding only a small header prefix. Returns `None` if the format/header
/// is not recognized, in which case callers fall back to a full decode.
fn dimensions_from_base64_header(data: &str) -> Option<(u32, u32)> {
    // 1024 base64 chars decode to ~768 bytes, enough for PNG/GIF/WebP headers and
    // a JPEG's leading SOF marker in the common (no giant leading EXIF) case.
    let trimmed = data.trim_start();
    let prefix_len = trimmed.len().min(2048);
    // base64 must be decoded on 4-char boundaries.
    let aligned = prefix_len - (prefix_len % 4);
    let prefix = &trimmed.as_bytes()[..aligned];
    let header = BASE64.decode(prefix).ok()?;
    dimensions_from_bytes(&header)
}

/// Read width/height from raw encoded image bytes (header-only, no decode).
pub fn dimensions_from_bytes(data: &[u8]) -> Option<(u32, u32)> {
    // PNG: signature + IHDR chunk.
    if data.len() > 24 && &data[0..8] == b"\x89PNG\r\n\x1a\n" {
        let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        return Some((width, height));
    }

    // JPEG: scan for an SOF marker.
    if data.len() > 2 && data[0] == 0xFF && data[1] == 0xD8 {
        let mut i = 2;
        while i + 9 < data.len() {
            if data[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = data[i + 1];
            // SOF0 (baseline) / SOF1 / SOF2 (progressive) all carry dimensions.
            if (0xC0..=0xC3).contains(&marker)
                || (0xC5..=0xC7).contains(&marker)
                || (0xC9..=0xCB).contains(&marker)
                || (0xCD..=0xCF).contains(&marker)
            {
                let height = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
                let width = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
                return Some((width, height));
            }
            if i + 3 < data.len() {
                let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
                i += 2 + len;
            } else {
                break;
            }
        }
    }

    // GIF.
    if data.len() > 10 && (&data[0..6] == b"GIF87a" || &data[0..6] == b"GIF89a") {
        let width = u16::from_le_bytes([data[6], data[7]]) as u32;
        let height = u16::from_le_bytes([data[8], data[9]]) as u32;
        return Some((width, height));
    }

    // WebP (RIFF container).
    if data.len() > 30 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        // Lossy VP8.
        if &data[12..16] == b"VP8 "
            && data[23] == 0x9D
            && data[24] == 0x01
            && data[25] == 0x2A
        {
            let width = (u16::from_le_bytes([data[26], data[27]]) & 0x3FFF) as u32;
            let height = (u16::from_le_bytes([data[28], data[29]]) & 0x3FFF) as u32;
            return Some((width, height));
        }
        // Lossless VP8L.
        if &data[12..16] == b"VP8L" && data.len() > 25 {
            let bits = u32::from_le_bytes([data[21], data[22], data[23], data[24]]);
            let width = (bits & 0x3FFF) + 1;
            let height = ((bits >> 14) & 0x3FFF) + 1;
            return Some((width, height));
        }
        // Extended VP8X.
        if &data[12..16] == b"VP8X" && data.len() > 30 {
            let width = (u32::from_le_bytes([data[24], data[25], data[26], 0]) & 0xFF_FFFF) + 1;
            let height = (u32::from_le_bytes([data[27], data[28], data[29], 0]) & 0xFF_FFFF) + 1;
            return Some((width, height));
        }
    }

    None
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    hash: u64,
    len: usize,
    max_edge: u32,
}

type ClampCache = Mutex<(Vec<CacheKey>, HashMap<CacheKey, (String, String)>)>;

fn cache() -> &'static ClampCache {
    static CACHE: OnceLock<ClampCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new((Vec::new(), HashMap::new())))
}

fn cache_get(key: &CacheKey) -> Option<(String, String)> {
    let guard = cache().lock().ok()?;
    guard.1.get(key).cloned()
}

fn cache_put(key: CacheKey, value: (String, String)) {
    if let Ok(mut guard) = cache().lock() {
        let (order, map) = &mut *guard;
        if map.contains_key(&key) {
            return;
        }
        if order.len() >= CACHE_CAPACITY {
            let evict = order.remove(0);
            map.remove(&evict);
        }
        order.push(key.clone());
        map.insert(key, value);
    }
}

fn hash_str(s: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageEncoder, RgbImage, Rgba, RgbaImage};

    fn encode_png(w: u32, h: u32) -> String {
        let img = RgbaImage::from_pixel(w, h, Rgba([10, 120, 200, 255]));
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .unwrap();
        BASE64.encode(&buf)
    }

    fn encode_jpeg(w: u32, h: u32) -> String {
        let img = RgbImage::from_pixel(w, h, image::Rgb([10, 120, 200]));
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, 90);
        encoder
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        BASE64.encode(&buf)
    }

    #[test]
    fn small_png_is_untouched() {
        let data = encode_png(100, 80);
        let (mt, out) = clamp_base64_image_with_edge("image/png", &data, 2000);
        assert!(matches!(mt, Cow::Borrowed(_)));
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), data);
    }

    #[test]
    fn header_dimension_parse_png() {
        let data = encode_png(2500, 1000);
        assert_eq!(dimensions_from_base64_header(&data), Some((2500, 1000)));
    }

    #[test]
    fn header_dimension_parse_jpeg() {
        let data = encode_jpeg(2400, 1200);
        assert_eq!(dimensions_from_base64_header(&data), Some((2400, 1200)));
    }

    #[test]
    fn oversized_png_is_downscaled_within_bounds() {
        let data = encode_png(4000, 2000);
        let (mt, out) = clamp_base64_image_with_edge("image/png", &data, 2000);
        assert!(matches!(out, Cow::Owned(_)));
        assert_eq!(mt.as_ref(), "image/png");
        let bytes = BASE64.decode(out.as_ref()).unwrap();
        let (w, h) = dimensions_from_bytes(&bytes).unwrap();
        assert!(w <= 2000 && h <= 2000, "got {w}x{h}");
        // Aspect ratio preserved: 4000x2000 -> 2000x1000.
        assert_eq!((w, h), (2000, 1000));
    }

    #[test]
    fn oversized_jpeg_stays_jpeg() {
        let data = encode_jpeg(3000, 1500);
        let (mt, out) = clamp_base64_image_with_edge("image/jpeg", &data, 2000);
        assert!(matches!(out, Cow::Owned(_)));
        assert_eq!(mt.as_ref(), "image/jpeg");
        let bytes = BASE64.decode(out.as_ref()).unwrap();
        let (w, h) = dimensions_from_bytes(&bytes).unwrap();
        assert!(w <= 2000 && h <= 2000, "got {w}x{h}");
    }

    #[test]
    fn second_call_is_served_from_cache() {
        let data = encode_png(5000, 2500);
        let (_, first) = clamp_base64_image_with_edge("image/png", &data, 2000);
        let (_, second) = clamp_base64_image_with_edge("image/png", &data, 2000);
        assert_eq!(first.as_ref(), second.as_ref());
    }

    #[test]
    fn undecodable_payload_falls_back_to_original() {
        let data = BASE64.encode(b"not really an image but big enough to look like base64 data");
        let (mt, out) = clamp_base64_image_with_edge("image/webp", &data, 2000);
        assert_eq!(mt.as_ref(), "image/webp");
        assert_eq!(out.as_ref(), data);
    }
}
