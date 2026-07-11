//! Progressive RAW preview decode for Develop mode.
//!
//! Phase 1: largest embedded JPEG (fast first paint).
//! Phase 2: LibRaw linear demosaic (via `develop_linear`) + tone (authoritative).

use std::io::{Read, Seek, SeekFrom};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use image::imageops::{self, FilterType};
use image::{DynamicImage, ImageBuffer, ImageFormat, ImageReader, RgbaImage};

/// Longest edge of the develop preview after downscale.
pub const PREVIEW_MAX_DIM: u32 = 2048;

/// JPEG SOI marker.
const JPEG_SOI: &[u8] = &[0xFF, 0xD8, 0xFF];

/// Bytes of the file to scan for an embedded JPEG preview.
const SCAN_LIMIT: u64 = 64 * 1024 * 1024;

/// Known camera-raw extensions (lowercase, no leading dot).
const RAW_EXTENSIONS: &[&str] = &[
    "cr2", "cr3", "crm", "crw", "dng", "nef", "nrw", "arw", "srf", "sr2", "rw2", "orf", "ori",
    "raf", "pef", "iiq", "3fr", "fff", "x3f", "mrw", "rwl", "srw", "r3d", "dcr", "kdc", "erf",
    "mef", "mos", "raw", "bay", "cap", "data", "dcs", "drf", "eip", "mdc", "obm", "ptx", "pxn",
    "r3d", "rwz",
];

/// Where the preview pixels came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewSource {
    /// Library disk cache (`Thumbnails/{shard}/{id}.jpg`).
    CachedThumb,
    /// Camera-embedded JPEG (or scanned JPEG block).
    Embedded,
    /// Demosaic preview loaded from the develop disk cache (`Previews/`).
    CachedPreview,
    /// Full rawler demosaic pipeline.
    Demosaic,
    /// Decoder-provided preview/full RGB image (fallback).
    DecoderPreview,
}

impl PreviewSource {
    /// `true` for authoritative develop previews (live or cached demosaic).
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            Self::CachedPreview | Self::Demosaic | Self::DecoderPreview
        )
    }
}

/// Decoded preview ready for an egui texture.
#[derive(Debug, Clone)]
pub struct PreviewImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub source: PreviewSource,
}

impl PreviewImage {
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Errors from develop preview decode.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("develop preview supports RAW files only")]
    NotRaw,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("image decode error: {0}")]
    Image(#[from] image::ImageError),

    #[error("jpeg decode error: {0}")]
    Jpeg(String),

    #[error("raw decode error: {0}")]
    Raw(String),

    #[error("no preview available: {0}")]
    NoPreview(String),
}

/// `true` if the path extension looks like a camera raw format.
pub fn is_raw_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|s| {
            let lower = s.to_ascii_lowercase();
            RAW_EXTENSIONS.iter().any(|e| *e == lower)
        })
        .unwrap_or(false)
}

/// Decode the largest embedded JPEG preview. Fast first paint.
///
/// Tries both EXIF-tagged previews and a file scan, then keeps the
/// largest by pixel count. EXIF alone often only points at a tiny
/// IFD1 thumbnail (~160×120); the large camera preview is usually
/// found by scanning.
pub fn decode_embedded_preview(
    path: &Path,
    orientation: Option<i64>,
) -> Result<PreviewImage, DecodeError> {
    if !is_raw_path(path) {
        return Err(DecodeError::NotRaw);
    }

    let mut best: Option<DynamicImage> = None;
    let mut best_pixels: u64 = 0;

    for candidate in [extract_embedded_jpeg(path), scan_largest_jpeg(path)] {
        if let Ok(img) = candidate {
            let pixels = img.width() as u64 * img.height() as u64;
            if pixels > best_pixels {
                best_pixels = pixels;
                best = Some(img);
            }
        }
    }

    let img = best.ok_or_else(|| DecodeError::NoPreview("no embedded JPEG preview".into()))?;
    finalize_image(img, orientation, PreviewSource::Embedded)
}

/// Full demosaic develop path via rawler. Authoritative preview.
pub fn decode_demosaic_preview(
    path: &Path,
    orientation: Option<i64>,
) -> Result<PreviewImage, DecodeError> {
    if !is_raw_path(path) {
        return Err(DecodeError::NotRaw);
    }

    match panic::catch_unwind(AssertUnwindSafe(|| demosaic_inner(path, orientation))) {
        Ok(result) => result,
        Err(_) => Err(DecodeError::Raw(
            "rawler demosaic panicked (unsupported or corrupt file)".into(),
        )),
    }
}

/// Progressive decode: try demosaic, then decoder RGB previews.
/// Used as Phase 2 after an optional embedded JPEG.
pub fn decode_raw_preview(
    path: &Path,
    orientation: Option<i64>,
) -> Result<PreviewImage, DecodeError> {
    if !is_raw_path(path) {
        return Err(DecodeError::NotRaw);
    }

    // Prefer full demosaic.
    match decode_demosaic_preview(path, orientation) {
        Ok(img) => return Ok(img),
        Err(e) => {
            eprintln!("demosaic failed for {}: {e}", path.display());
        }
    }

    // Fall back to decoder-provided RGB images.
    match panic::catch_unwind(AssertUnwindSafe(|| decoder_rgb_preview(path, orientation))) {
        Ok(Ok(img)) => Ok(img),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(DecodeError::Raw(
            "rawler decoder preview panicked".into(),
        )),
    }
}

fn demosaic_inner(path: &Path, orientation: Option<i64>) -> Result<PreviewImage, DecodeError> {
    let raw = rawler::decode_file(path).map_err(|e| DecodeError::Raw(e.to_string()))?;

    let ori = orientation.or_else(|| {
        let u = raw.orientation.to_u16();
        if u == 0 {
            None
        } else {
            Some(u as i64)
        }
    });

    let dev = rawler::imgop::develop::RawDevelop::default();
    let intermediate = dev
        .develop_intermediate(&raw)
        .map_err(|e| DecodeError::Raw(e.to_string()))?;

    let dyn_img = intermediate
        .to_dynamic_image()
        .ok_or_else(|| DecodeError::Raw("failed to convert developed image".into()))?;

    finalize_image(dyn_img, ori, PreviewSource::Demosaic)
}

fn decoder_rgb_preview(
    path: &Path,
    orientation: Option<i64>,
) -> Result<PreviewImage, DecodeError> {
    use rawler::decoders::RawDecodeParams;
    use rawler::rawsource::RawSource;

    let rawfile = RawSource::new(path).map_err(|e| DecodeError::Raw(e.to_string()))?;
    let decoder = rawler::get_decoder(&rawfile).map_err(|e| DecodeError::Raw(e.to_string()))?;
    let params = RawDecodeParams::default();

    let dyn_img = decoder
        .preview_image(&rawfile, &params)
        .map_err(|e| DecodeError::Raw(e.to_string()))?
        .or_else(|| {
            decoder
                .full_image(&rawfile, &params)
                .ok()
                .flatten()
        })
        .ok_or_else(|| DecodeError::NoPreview("no decoder RGB preview".into()))?;

    finalize_image(dyn_img, orientation, PreviewSource::DecoderPreview)
}

fn finalize_image(
    img: DynamicImage,
    orientation: Option<i64>,
    source: PreviewSource,
) -> Result<PreviewImage, DecodeError> {
    let oriented = apply_orientation(img, orientation.unwrap_or(1));
    let resized = downscale(oriented, PREVIEW_MAX_DIM);
    let rgba: RgbaImage = resized.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(PreviewImage {
        width,
        height,
        rgba: rgba.into_raw(),
        source,
    })
}

fn downscale(img: DynamicImage, max_dim: u32) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    if w <= max_dim && h <= max_dim {
        return img;
    }
    img.resize(max_dim, max_dim, FilterType::Triangle)
}

/// Apply EXIF orientation (1–8) to a [`DynamicImage`].
fn apply_orientation(img: DynamicImage, orientation: i64) -> DynamicImage {
    let rgba = img.to_rgba8();
    let oriented = match orientation {
        2 => imageops::flip_horizontal(&rgba),
        3 => imageops::rotate180(&rgba),
        4 => imageops::flip_vertical(&rgba),
        5 => {
            let t = imageops::rotate90(&rgba);
            imageops::flip_horizontal(&t)
        }
        6 => imageops::rotate90(&rgba),
        7 => {
            let t = imageops::rotate90(&rgba);
            imageops::flip_vertical(&t)
        }
        8 => imageops::rotate270(&rgba),
        _ => rgba,
    };
    DynamicImage::ImageRgba8(oriented)
}

fn extract_embedded_jpeg(path: &Path) -> Result<DynamicImage, String> {
    use exif::{Reader, Tag};

    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let exif = {
        let mut reader = std::io::BufReader::new(&file);
        Reader::new()
            .read_from_container(&mut reader)
            .map_err(|e| e.to_string())?
    };

    let mut candidates: Vec<(u64, u64)> = Vec::new();
    for f in exif.fields() {
        if f.tag != Tag::JPEGInterchangeFormat {
            continue;
        }
        let Some(offset) = f.value.get_uint(0) else {
            continue;
        };
        let length = exif
            .fields()
            .find(|g| g.tag == Tag::JPEGInterchangeFormatLength && g.ifd_num == f.ifd_num)
            .and_then(|g| g.value.get_uint(0))
            .unwrap_or(0);
        if length == 0 {
            continue;
        }
        candidates.push((offset as u64, length as u64));
    }

    // Prefer larger blobs (likely the full preview, not the tiny CFA thumb).
    candidates.sort_by(|a, b| b.1.cmp(&a.1));

    for (offset, length) in candidates {
        let mut buf = vec![0u8; length as usize];
        if file.seek(SeekFrom::Start(offset)).is_err() {
            continue;
        }
        if file.read_exact(&mut buf).is_err() {
            continue;
        }
        if !buf.starts_with(JPEG_SOI) {
            continue;
        }
        if let Ok(img) = decode_jpeg_bytes(&buf) {
            return Ok(img);
        }
    }

    Err("no embedded JPEG via EXIF".into())
}

fn scan_largest_jpeg(path: &Path) -> Result<DynamicImage, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let total = file.metadata().map_err(|e| e.to_string())?.len();
    let to_scan = total.min(SCAN_LIMIT);
    let mut buf = vec![0u8; to_scan as usize];
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    file.read_exact(&mut buf).map_err(|e| e.to_string())?;

    let mut best: Option<(usize, usize)> = None;
    let mut i = 0;
    while i + 3 <= buf.len() {
        if &buf[i..i + 3] == JPEG_SOI {
            let mut j = i + 3;
            let mut found_eoi = None;
            while j + 1 < buf.len() {
                if buf[j] == 0xFF && buf[j + 1] == 0xD9 {
                    found_eoi = Some(j + 2);
                    break;
                }
                j += 1;
            }
            if let Some(end) = found_eoi {
                let len = end - i;
                if best.is_none_or(|(_, b_len)| len > b_len) {
                    best = Some((i, end));
                }
                i = end;
            } else {
                let len = buf.len() - i;
                if best.is_none_or(|(_, b_len)| len > b_len) {
                    best = Some((i, buf.len()));
                }
                break;
            }
        } else {
            i += 1;
        }
    }

    let Some((start, end)) = best else {
        return Err("no JPEG in scan".into());
    };
    // Ignore tiny JPEG blobs (often < 10 KB CFA-related junk).
    if end - start < 10_000 {
        return Err("JPEG too small".into());
    }
    decode_jpeg_bytes(&buf[start..end])
}

fn decode_jpeg_bytes(bytes: &[u8]) -> Result<DynamicImage, String> {
    if let Ok(img) = decode_jpeg_native(bytes) {
        return Ok(img);
    }
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes));
    reader.set_format(ImageFormat::Jpeg);
    reader.decode().map_err(|e| e.to_string())
}

fn decode_jpeg_native(bytes: &[u8]) -> Result<DynamicImage, String> {
    let mut decoder = jpeg_decoder::Decoder::new(bytes);
    // Request a scale that keeps the long edge near PREVIEW_MAX_DIM.
    let _ = decoder.scale(PREVIEW_MAX_DIM as u16, PREVIEW_MAX_DIM as u16);
    let pixels = decoder.decode().map_err(|e| e.to_string())?;
    let info = decoder.info().ok_or_else(|| "no jpeg info".to_string())?;
    let w = info.width as u32;
    let h = info.height as u32;

    let rgba = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            let mut out = Vec::with_capacity(pixels.len() / 3 * 4);
            for chunk in pixels.chunks_exact(3) {
                out.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
            out
        }
        jpeg_decoder::PixelFormat::L8 => {
            let mut out = Vec::with_capacity(pixels.len() * 4);
            for &p in &pixels {
                out.extend_from_slice(&[p, p, p, 255]);
            }
            out
        }
        jpeg_decoder::PixelFormat::L16 => {
            let mut out = Vec::with_capacity(pixels.len() / 2 * 4);
            for chunk in pixels.chunks_exact(2) {
                let p = chunk[0]; // high byte is fine for preview
                out.extend_from_slice(&[p, p, p, 255]);
            }
            out
        }
        jpeg_decoder::PixelFormat::CMYK32 => {
            let mut out = Vec::with_capacity(pixels.len());
            for chunk in pixels.chunks_exact(4) {
                let c = chunk[0] as u32;
                let m = chunk[1] as u32;
                let y = chunk[2] as u32;
                let k = chunk[3] as u32;
                let r = 255 - ((c * (255 - k) / 255 + k) as u8);
                let g = 255 - ((m * (255 - k) / 255 + k) as u8);
                let b = 255 - ((y * (255 - k) / 255 + k) as u8);
                out.extend_from_slice(&[r, g, b, 255]);
            }
            out
        }
    };

    let buf: ImageBuffer<image::Rgba<u8>, _> =
        ImageBuffer::from_raw(w, h, rgba).ok_or_else(|| "invalid jpeg buffer".to_string())?;
    Ok(DynamicImage::ImageRgba8(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn raw_extension_detection() {
        assert!(is_raw_path(Path::new("IMG_0001.CR2")));
        assert!(is_raw_path(Path::new("a.nef")));
        assert!(is_raw_path(Path::new("x.ARW")));
        assert!(is_raw_path(Path::new("y.dng")));
        assert!(is_raw_path(Path::new("z.cr3")));
        assert!(!is_raw_path(Path::new("photo.jpg")));
        assert!(!is_raw_path(Path::new("photo.png")));
        assert!(!is_raw_path(Path::new("photo.tif")));
        assert!(!is_raw_path(Path::new("noext")));
    }

    #[test]
    fn not_raw_returns_error() {
        let p = PathBuf::from("foo.jpg");
        assert!(matches!(
            decode_embedded_preview(&p, None),
            Err(DecodeError::NotRaw)
        ));
        assert!(matches!(
            decode_demosaic_preview(&p, None),
            Err(DecodeError::NotRaw)
        ));
    }

    #[test]
    fn orientation_normal_preserves_size() {
        let img = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            4,
            2,
            image::Rgba([10, 20, 30, 255]),
        ));
        let out = apply_orientation(img, 1);
        assert_eq!(out.width(), 4);
        assert_eq!(out.height(), 2);
    }

    #[test]
    fn orientation_rotate90_swaps_dims() {
        let img = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            4,
            2,
            image::Rgba([10, 20, 30, 255]),
        ));
        let out = apply_orientation(img, 6);
        assert_eq!(out.width(), 2);
        assert_eq!(out.height(), 4);
    }
}
