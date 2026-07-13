//! Thumbnail extraction from raw and standard image files.
//!
//! ## Strategy
//!
//! 1. **Largest embedded JPEG**: collect every EXIF-tagged
//!    `JPEGInterchangeFormat` blob *and* every JPEG found by scanning
//!    the file body, then decode the largest one. Cameras almost
//!    always expose a tiny IFD1 thumb (~160×120) via EXIF and a much
//!    larger preview (~1620×1080) only as an untagged blob — taking
//!    the first EXIF hit is wrong.
//! 2. **rawler decoder preview**: camera-aware RGB preview/thumbnail
//!    (covers formats where JPEG scanning fails, e.g. some CR3s).
//! 3. **Full-file decode** for JPEGs, PNGs, and TIFFs.
//! 4. **rawler CFA fallback**: undemosaiced grayscale last resort.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use exif::{Reader, Tag};
use image::imageops::FilterType;
use image::{ImageFormat, ImageReader, RgbaImage};

/// Pixel size of the longest edge of every cell in the grid.
pub const THUMB_CELL: f32 = 156.0;

/// JPEG SOI (start of image) marker: `FF D8 FF`.
const JPEG_SOI: &[u8] = &[0xFF, 0xD8, 0xFF];

/// How many bytes of the file to scan when hunting for an embedded JPEG.
/// ~64 MiB is enough for any preview; bigger scans just slow us down.
const SCAN_LIMIT: u64 = 64 * 1024 * 1024;

/// A decoded thumbnail ready to be uploaded to the GPU.
#[derive(Debug, Clone)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// The largest dimension we asked for. Useful for layout.
    pub max_dim: u32,
}

impl Thumbnail {
    /// Convenience: `true` if no pixels are set (only possible on an
    /// empty image).
    pub fn is_empty(&self) -> bool {
        self.rgba.is_empty()
    }

    /// Width / height, as a tuple.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Target longest edge for library / disk-cache thumbnails.
pub const THUMB_MAX_DIM: u32 = 1024;

/// Target longest edge for import-dialog grid thumbs (cells are ~156 px).
pub const DIALOG_THUMB_MAX_DIM: u32 = 256;

/// EXIF JPEG blobs at least this large are treated as real previews
/// (not tiny IFD1 thumbs), so we can skip the expensive 64 MiB file scan.
const GOOD_PREVIEW_MIN_BYTES: u64 = 32 * 1024;

/// Errors from thumbnail extraction.
#[derive(Debug, thiserror::Error)]
pub enum ThumbnailError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("image decode error: {0}")]
    Image(#[from] image::ImageError),

    #[error("file format is not supported for thumbnails")]
    Unsupported,

    #[error("jpeg decoder error: {0}")]
    JpegDecode(jpeg_decoder::Error),

    #[error("no embedded preview and full-file decode failed: {0}")]
    NoEmbedded(String),

    #[error("exif parse error: {0}")]
    Exif(String),

    #[error("raw decode error: {0}")]
    RawDecode(String),
}

/// Try every strategy to produce a thumbnail. Returns `Err` only if
/// nothing worked.
pub fn extract_thumbnail(path: &Path) -> Result<Thumbnail, ThumbnailError> {
    extract_thumbnail_sized(path, THUMB_MAX_DIM)
}

/// Extract a preview for the import dialog: same strategies as
/// [`extract_thumbnail`], but capped at [`DIALOG_THUMB_MAX_DIM`] so the
/// grid stays responsive.
pub fn extract_dialog_preview(path: &Path) -> Result<Thumbnail, ThumbnailError> {
    extract_thumbnail_sized(path, DIALOG_THUMB_MAX_DIM)
}

fn extract_thumbnail_sized(path: &Path, max_dim: u32) -> Result<Thumbnail, ThumbnailError> {
    // Strategy 1: largest embedded JPEG (EXIF tags + optional file scan).
    if let Ok(t) = extract_largest_preview_jpeg(path, max_dim) {
        return orient_thumbnail(path, t);
    }

    // Strategy 2: rawler's camera-aware RGB preview (CR3, etc.).
    if let Ok(t) = extract_rawler_preview(path, max_dim) {
        return orient_thumbnail(path, t);
    }

    // Strategy 3: full-file decode for JPEGs and other supported types.
    if let Ok(t) = extract_full(path, max_dim) {
        return orient_thumbnail(path, t);
    }

    // Strategy 4: raw sensor decode via rawler (undemosaiced grayscale).
    let t = extract_raw_thumbnail(path, max_dim)?;
    orient_thumbnail(path, t)
}

/// Apply EXIF orientation to a thumbnail. Returns the original thumbnail
/// unchanged when no orientation tag is present or orientation is 1/0.
fn orient_thumbnail(path: &Path, thumb: Thumbnail) -> Result<Thumbnail, ThumbnailError> {
    let Some(ori) = read_orientation(path) else {
        return Ok(thumb);
    };
    if ori == 1 || ori == 0 {
        return Ok(thumb);
    }
    let max_dim = thumb.max_dim;
    let img = image::RgbaImage::from_raw(thumb.width, thumb.height, thumb.rgba)
        .ok_or_else(|| ThumbnailError::Unsupported)?;
    let oriented: image::RgbaImage = match ori {
        2 => image::imageops::flip_horizontal(&img),
        3 => image::imageops::rotate180(&img),
        4 => image::imageops::flip_vertical(&img),
        5 => {
            let t = image::imageops::rotate90(&img);
            image::imageops::flip_horizontal(&t)
        }
        6 => image::imageops::rotate90(&img),
        7 => {
            let t = image::imageops::rotate90(&img);
            image::imageops::flip_vertical(&t)
        }
        8 => image::imageops::rotate270(&img),
        _ => img,
    };
    // Re-resize after orientation (rotations may change the long edge).
    let resized = if oriented.width().max(oriented.height()) > max_dim {
        image::DynamicImage::ImageRgba8(oriented).thumbnail(max_dim, max_dim)
    } else {
        image::DynamicImage::ImageRgba8(oriented)
    };
    let rgba = resized.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(Thumbnail {
        width: w,
        height: h,
        rgba: rgba.into_raw(),
        max_dim,
    })
}

/// Read EXIF orientation from a file. Returns `None` when EXIF is absent
/// or unreadable (treats parse errors as missing — every file that lacks
/// a camera EXIF block returns `None`, which is the common case for
/// non-raw, non-JPEG files).
fn read_orientation(path: &Path) -> Option<i64> {
    use exif::{In, Reader, Tag};
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let exif = Reader::new().read_from_container(&mut reader).ok()?;
    exif.get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .map(|v| v as i64)
}

/// Collect every EXIF-tagged JPEG (and, if needed, scanned JPEG blobs),
/// then decode the largest by byte length.
///
/// When EXIF already points at a large preview (≥ [`GOOD_PREVIEW_MIN_BYTES`]),
/// the expensive full-file scan is skipped.
pub fn extract_largest_preview_jpeg(
    path: &Path,
    max_dim: u32,
) -> Result<Thumbnail, ThumbnailError> {
    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();

    // (offset, length) candidates; length is used as the quality proxy.
    let mut candidates: Vec<(u64, u64)> = Vec::new();

    // --- EXIF-tagged previews ---
    if let Ok(exif) = {
        let mut reader = std::io::BufReader::new(&file);
        Reader::new().read_from_container(&mut reader)
    } {
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
    }

    let best_exif = candidates.iter().map(|(_, len)| *len).max().unwrap_or(0);
    // Only scan the file body when EXIF has no usable large preview.
    // Scanning up to 64 MiB per raw is the main import-dialog cost.
    if best_exif < GOOD_PREVIEW_MIN_BYTES {
        let to_scan = file_len.min(SCAN_LIMIT);
        let mut scan_buf = vec![0u8; to_scan as usize];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut scan_buf)?;
        for (start, end) in find_jpeg_spans(&scan_buf) {
            candidates.push((start as u64, (end - start) as u64));
        }
    }

    if candidates.is_empty() {
        return Err(ThumbnailError::Exif("no embedded JPEG candidates".into()));
    }

    // Prefer larger blobs (full previews over 160×120 IFD1 thumbs).
    candidates.sort_by_key(|b| std::cmp::Reverse(b.1));

    // Try candidates largest-first; skip non-JPEG / undecodable blobs.
    let mut last_err = ThumbnailError::Exif("no decodable embedded JPEG".into());
    for (offset, length) in candidates {
        if offset >= file_len || length == 0 {
            continue;
        }
        let read_len = length.min(file_len - offset) as usize;
        let mut buf = vec![0u8; read_len];
        if file.seek(SeekFrom::Start(offset)).is_err() {
            continue;
        }
        if file.read_exact(&mut buf).is_err() {
            continue;
        }
        if !buf.starts_with(JPEG_SOI) {
            continue;
        }
        match decode_jpeg(&buf, max_dim) {
            Ok(t) => return Ok(t),
            Err(e) => last_err = e,
        }
    }

    Err(last_err)
}

/// Try every IFD's `JPEGInterchangeFormat` tag, largest first.
/// Prefer [`extract_largest_preview_jpeg`] which also scans the file.
pub fn extract_embedded(path: &Path) -> Result<Thumbnail, ThumbnailError> {
    extract_largest_preview_jpeg(path, THUMB_MAX_DIM)
}

/// Scan the first [`SCAN_LIMIT`] bytes of the file for the largest
/// contiguous JPEG block and decode it.
pub fn scan_for_largest_jpeg(path: &Path) -> Result<Thumbnail, ThumbnailError> {
    let mut file = std::fs::File::open(path)?;
    let total = file.metadata()?.len();
    let to_scan = total.min(SCAN_LIMIT);
    let mut buf = vec![0u8; to_scan as usize];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut buf)?;

    let mut best: Option<(usize, usize)> = None;
    for (start, end) in find_jpeg_spans(&buf) {
        let len = end - start;
        if best.is_none_or(|(_, b_len)| len > b_len) {
            best = Some((start, end));
        }
    }

    let Some((start, end)) = best else {
        return Err(ThumbnailError::Exif("no JPEG in scan".into()));
    };
    decode_jpeg(&buf[start..end], THUMB_MAX_DIM)
}

/// Find every JPEG SOI…EOI span in `buf`. Returns `(start, end)` half-open ranges.
fn find_jpeg_spans(buf: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
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
                spans.push((i, end));
                i = end;
            } else {
                spans.push((i, buf.len()));
                break;
            }
        } else {
            i += 1;
        }
    }
    spans
}

/// Extract a camera RGB preview via rawler (no demosaic). Used when
/// JPEG scanning misses vendor-specific containers (CR3, etc.).
fn extract_rawler_preview(path: &Path, max_dim: u32) -> Result<Thumbnail, ThumbnailError> {
    use rawler::decoders::RawDecodeParams;
    use rawler::rawsource::RawSource;

    let rawfile = RawSource::new(path).map_err(|e| ThumbnailError::RawDecode(e.to_string()))?;
    let decoder =
        rawler::get_decoder(&rawfile).map_err(|e| ThumbnailError::RawDecode(e.to_string()))?;
    let params = RawDecodeParams::default();

    // Prefer the larger preview over the tiny thumbnail when available.
    // Avoid full_image for dialog-sized thumbs — it can be huge.
    let img = if max_dim <= DIALOG_THUMB_MAX_DIM {
        decoder
            .preview_image(&rawfile, &params)
            .ok()
            .flatten()
            .or_else(|| decoder.thumbnail_image(&rawfile, &params).ok().flatten())
    } else {
        decoder
            .preview_image(&rawfile, &params)
            .ok()
            .flatten()
            .or_else(|| decoder.full_image(&rawfile, &params).ok().flatten())
            .or_else(|| decoder.thumbnail_image(&rawfile, &params).ok().flatten())
    }
    .ok_or_else(|| ThumbnailError::RawDecode("no rawler RGB preview".into()))?;

    let resized = img.thumbnail(max_dim, max_dim);
    let rgba = resized.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(Thumbnail {
        width: w,
        height: h,
        rgba: rgba.into_raw(),
        max_dim,
    })
}

fn decode_jpeg(bytes: &[u8], max_dim: u32) -> Result<Thumbnail, ThumbnailError> {
    // Fast path: jpeg-decoder's native `scale` is 10-50x faster
    // than the image crate's full decode for sources much larger
    // than max_dim (common for raw previews).
    if let Ok(t) = decode_jpeg_native(bytes, max_dim) {
        return Ok(t);
    }
    // Fallback: force JPEG format so we never call into the
    // TIFF / WebP / etc. decoders.
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes));
    reader.set_format(ImageFormat::Jpeg);
    let img = reader.decode()?;
    let resized = img.resize(max_dim, max_dim, FilterType::Triangle);
    let rgba: RgbaImage = resized.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(Thumbnail {
        width: w,
        height: h,
        rgba: rgba.into_raw(),
        max_dim,
    })
}

/// jpeg-decoder's scaled JPEG path for in-memory bytes. Returns
/// `Err` if the bytes aren't a JPEG or the decoder can't handle
/// the colour format.
fn decode_jpeg_native(bytes: &[u8], max_dim: u32) -> Result<Thumbnail, ThumbnailError> {
    let mut decoder = jpeg_decoder::Decoder::new(bytes);
    let _ = decoder.scale(max_dim as u16, max_dim as u16);
    let pixels = decoder.decode().map_err(ThumbnailError::JpegDecode)?;
    let info = decoder.info().ok_or(ThumbnailError::Unsupported)?;
    let w = info.width as u32;
    let h = info.height as u32;
    let rgba = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => rgb_to_rgba(&pixels),
        jpeg_decoder::PixelFormat::L8 => l_to_rgba(&pixels),
        jpeg_decoder::PixelFormat::L16 => l16_to_rgba(&pixels),
        jpeg_decoder::PixelFormat::CMYK32 => cmyk_to_rgba(&pixels),
    };
    Ok(Thumbnail {
        width: w,
        height: h,
        rgba,
        max_dim,
    })
}

/// Decode the file to a thumbnail-sized image and resize. Used as a
/// fallback for files without an embedded preview (most JPEGs, PNGs,
/// etc.).
fn extract_full(path: &Path, max_dim: u32) -> Result<Thumbnail, ThumbnailError> {
    if is_jpeg(path)
        && let Ok(t) = extract_jpeg_scaled(path, max_dim)
    {
        return Ok(t);
    }

    // Generic path: full decode, then a fast integer-only
    // thumbnail. `image::DynamicImage::thumbnail` uses
    // `imageops::sample::thumbnail` (one source pixel per output
    // pixel) -- no filter, no floating point.
    let mut reader = ImageReader::open(path)?.with_guessed_format()?;
    reader.no_limits();
    let img = reader.decode()?;
    let thumb = img.thumbnail(max_dim, max_dim);
    let rgba = thumb.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(Thumbnail {
        width: w,
        height: h,
        rgba: rgba.into_raw(),
        max_dim,
    })
}

/// Decode a camera raw file (X3F, ORF, MRW, …) via rawler and
/// produce a grayscale thumbnail from the raw sensor data.
/// This is used as a last-resort fallback when no embedded JPEG
/// preview is found and the image crate can't decode the format.
fn extract_raw_thumbnail(path: &Path, max_dim: u32) -> Result<Thumbnail, ThumbnailError> {
    let img = rawler::decode_file(path).map_err(|e| ThumbnailError::RawDecode(e.to_string()))?;

    let (sw, sh) = (img.width, img.height);
    if sw == 0 || sh == 0 {
        return Err(ThumbnailError::RawDecode("empty raw image".into()));
    }

    let data = match &img.data {
        rawler::RawImageData::Integer(d) => d,
        _ => return Err(ThumbnailError::RawDecode("floating-point raw data not supported".into())),
    };

    // Nearest-neighbour downscale straight from the raw sensor
    // samples.  At thumbnail scale the result is perfectly
    // recognisable even without demosaicing.
    let scale = (max_dim as f32 / sw.max(sh) as f32).min(1.0);
    let dw = (sw as f32 * scale).ceil() as u32;
    let dh = (sh as f32 * scale).ceil() as u32;
    let mut rgba = vec![0u8; (dw * dh * 4) as usize];

    for dy in 0..dh {
        let sy = ((dy as f32) / scale) as usize;
        for dx in 0..dw {
            let sx = ((dx as f32) / scale) as usize;
            let src_idx = sy * sw + sx;
            let val = data.get(src_idx).map(|v| (v >> 8) as u8).unwrap_or(0);
            let dst_idx = (dy * dw + dx) as usize * 4;
            rgba[dst_idx] = val;
            rgba[dst_idx + 1] = val;
            rgba[dst_idx + 2] = val;
            rgba[dst_idx + 3] = 255;
        }
    }

    Ok(Thumbnail {
        width: dw,
        height: dh,
        rgba,
        max_dim,
    })
}

/// True if the file's extension looks like JPEG. We use the
/// extension rather than reading magic bytes because `extract_full`
/// is only called for files that already failed the embedded-JPEG
/// path -- a quick extension check is enough to decide whether to
/// try the JPEG-scaled shortcut.
fn is_jpeg(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("jpg" | "jpeg" | "jpe")
    )
}

/// Decode a JPEG to a thumbnail-sized image using libjpeg's native
/// `scale` (1/8, 1/4, 1/2, 1). The resulting image is at most the
/// requested size; we accept that and let the renderer's
/// size-to-fit logic letterbox it into the 3:2 card frame.
fn extract_jpeg_scaled(path: &Path, max_dim: u32) -> Result<Thumbnail, ThumbnailError> {
    let file = std::fs::File::open(path)?;
    let mut decoder = jpeg_decoder::Decoder::new(std::io::BufReader::new(file));
    // Ask for at most max_dim on the long edge; the decoder will pick
    // the smallest supported scale factor (1/8, 1/4, 1/2 or 1) that
    // produces an image >= that size in at least one axis.
    let _ = decoder.scale(max_dim as u16, max_dim as u16);
    let pixels = decoder.decode().map_err(ThumbnailError::JpegDecode)?;
    let info = decoder.info().ok_or(ThumbnailError::Unsupported)?;
    let w = info.width as u32;
    let h = info.height as u32;
    let rgba = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => rgb_to_rgba(&pixels),
        jpeg_decoder::PixelFormat::L8 => l_to_rgba(&pixels),
        jpeg_decoder::PixelFormat::L16 => l16_to_rgba(&pixels),
        jpeg_decoder::PixelFormat::CMYK32 => cmyk_to_rgba(&pixels),
    };
    Ok(Thumbnail {
        width: w,
        height: h,
        rgba,
        max_dim,
    })
}

fn rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
    for chunk in rgb.chunks_exact(3) {
        out.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
    }
    out
}

fn l_to_rgba(l: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(l.len() * 4);
    for &p in l {
        out.extend_from_slice(&[p, p, p, 255]);
    }
    out
}

fn l16_to_rgba(l: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(l.len() / 2 * 4);
    for chunk in l.chunks_exact(2) {
        let p = u16::from_be_bytes([chunk[0], chunk[1]]) as u8;
        out.extend_from_slice(&[p, p, p, 255]);
    }
    out
}

fn cmyk_to_rgba(cmyk: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(cmyk.len() / 4 * 4);
    for chunk in cmyk.chunks_exact(4) {
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

/// Lightweight sanity check: the file's extension looks like a known
/// raw or image format we might be able to handle. The error is purely
/// informational — [`extract_thumbnail`] will return
/// `Unsupported` if we genuinely cannot decode.
pub fn is_known_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let lower = ext.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "cr2" | "cr3"
            | "dng"
            | "nef"
            | "nrw"
            | "arw"
            | "srf"
            | "sr2"
            | "rw2"
            | "orf"
            | "raf"
            | "pef"
            | "iiq"
            | "mrw"
            | "srw"
            | "rwl"
            | "r3d"
            | "jpg"
            | "jpeg"
            | "jpe"
            | "tif"
            | "tiff"
            | "png"
            | "heic"
            | "heif"
            | "avif"
            | "webp"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_png(path: &Path, w: u32, h: u32) {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        img.save(path).unwrap();
    }

    /// Build a JPEG into a `Vec<u8>` for embedding into a fake TIFF.
    fn build_jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, _| {
            image::Rgb([(x * 32) as u8, 0, 0])
        });
        let dyn_img = image::DynamicImage::ImageRgb8(img);
        let mut buf = std::io::Cursor::new(Vec::<u8>::new());
        dyn_img
            .write_to(&mut buf, image::ImageFormat::Jpeg)
            .unwrap();
        buf.into_inner()
    }

    /// Build a fake "raw" file: a JPEG payload at a known offset,
    /// wrapped in TIFF scaffolding so kamadak-exif will read it. We
    /// hand-craft the smallest possible TIFF (single IFD, single
    /// tag) so the test doesn't depend on a real camera file.
    ///
    /// Returns the offset of the embedded JPEG inside the file.
    fn write_fake_tiff_with_jpeg(
        path: &Path,
        jpeg: &[u8],
    ) -> u64 {
        use std::io::Write;
        // Little-endian TIFF header.
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"II"); // byte order
        buf.extend_from_slice(&42u16.to_le_bytes()); // magic
        buf.extend_from_slice(&8u32.to_le_bytes()); // offset to IFD0
        // IFD0 with two entries: image width (256) + JPEGInterchangeFormat
        // (0x0201) + JPEGInterchangeFormatLength (0x0202). The thumbnail
        // length is required for kamadak-exif to consider this valid.
        //
        // IFD layout: 2 bytes (count) + 12 bytes per entry + 4 bytes
        // (next IFD offset = 0).
        let count: u16 = 3;
        let ifd_offset: u32 = 8;
        let entries_offset = ifd_offset + 2;
        let next_ifd_offset = entries_offset + count as u32 * 12 + 4;
        buf.resize(next_ifd_offset as usize, 0);

        // Helper to write an IFD entry at a given index.
        let mut write_entry = |idx: u16, tag: u16, kind: u16, count: u32, value: u32| {
            let pos = entries_offset as usize + idx as usize * 12;
            buf[pos..pos + 2].copy_from_slice(&tag.to_le_bytes());
            buf[pos + 2..pos + 4].copy_from_slice(&kind.to_le_bytes());
            buf[pos + 4..pos + 8].copy_from_slice(&count.to_le_bytes());
            buf[pos + 8..pos + 12].copy_from_slice(&value.to_le_bytes());
        };
        // We deliberately point to image dimensions in a later entry;
        // for this test the JPEG offset/length are what matters.
        write_entry(0, 0x0100, 3, 1, 64); // ImageWidth = 64 (SHORT)
        write_entry(1, 0x0201, 4, 1, next_ifd_offset); // JPEGInterchangeFormat
        write_entry(2, 0x0202, 4, 1, jpeg.len() as u32); // JPEGInterchangeFormatLength

        // IFD header.
        buf[ifd_offset as usize..ifd_offset as usize + 2]
            .copy_from_slice(&count.to_le_bytes());
        // Next-IFD offset = 0.
        let nxt = entries_offset + count as u32 * 12;
        buf[nxt as usize..nxt as usize + 4].copy_from_slice(&0u32.to_le_bytes());

        buf.extend_from_slice(jpeg);
        std::fs::File::create(path)
            .unwrap()
            .write_all(&buf)
            .unwrap();
        next_ifd_offset as u64
    }

    #[test]
    fn known_extensions_recognised() {
        assert!(is_known_extension(Path::new("/x/foo.cr2")));
        assert!(is_known_extension(Path::new("/x/foo.CR2")));
        assert!(is_known_extension(Path::new("/x/foo.dng")));
        assert!(is_known_extension(Path::new("/x/foo.jpg")));
        assert!(!is_known_extension(Path::new("/x/foo.txt")));
    }

    #[test]
    fn full_decode_of_png_works() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.png");
        write_png(&p, 400, 300);
        let t = extract_thumbnail(&p).expect("thumbnail");
        assert!(t.width <= THUMB_MAX_DIM);
        assert!(t.height <= THUMB_MAX_DIM);
        let expected_ratio = 400.0 / 300.0;
        let actual_ratio = t.width as f32 / t.height as f32;
        assert!(
            (expected_ratio - actual_ratio).abs() < 0.05,
            "ratio drifted: {expected_ratio} vs {actual_ratio}"
        );
    }

    #[test]
    fn no_exif_jpeg_falls_back_to_full_decode() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.jpg");
        let mut img = image::RgbImage::new(1, 1);
        img.put_pixel(0, 0, image::Rgb([0, 0, 0]));
        img.save(&p).unwrap();
        let t = extract_thumbnail(&p).expect("thumbnail");
        // jpeg-decoder's native scale never upscales, so a 1x1
        // source stays 1x1. The renderer's size-to-fit letterboxes
        // it into the 3:2 card frame.
        assert_eq!(t.width, 1);
        assert_eq!(t.height, 1);
    }

    #[test]
    fn unknown_file_returns_error() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.txt");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"hello").unwrap();
        let result = extract_thumbnail(&p);
        assert!(result.is_err());
    }

    #[test]
    fn scan_finds_jpeg_inside_arbitrary_file() {
        // Build a file with a JPEG block embedded after a bunch of
        // non-JPEG bytes; scan_for_largest_jpeg should find it.
        let dir = tempdir().unwrap();
        let p = dir.path().join("x.bin");

        let jpeg_buf = build_jpeg(8, 8);
        let mut all = Vec::new();
        all.extend_from_slice(b"some random prefix data -- not jpeg --\n");
        all.extend_from_slice(&jpeg_buf);
        all.extend_from_slice(b"trailing junk");
        std::fs::File::create(&p).unwrap().write_all(&all).unwrap();

        let t = scan_for_largest_jpeg(&p).expect("should find jpeg");
        // jpeg-decoder's native scale never upscales, so the
        // 8x8 source stays 8x8.
        assert_eq!(t.width, 8);
        assert_eq!(t.height, 8);
    }

    #[test]
    fn embedded_jpeg_decoded_via_exif_tag() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("fake_raw.bin");
        let jpeg_buf = build_jpeg(8, 8);
        write_fake_tiff_with_jpeg(&p, &jpeg_buf);
        // Even with no real CR2/DNG layout, the scan fallback should
        // rescue us if the EXIF path fails.
        let t = extract_thumbnail(&p).expect("thumbnail");
        assert!(t.width > 0 && t.height > 0);
    }

    #[test]
    fn prefers_larger_scanned_jpeg_over_tiny_exif_thumb() {
        // Simulate a real raw: tiny EXIF-tagged thumb + much larger
        // untagged preview later in the file. Library thumbs must use
        // the large one.
        let dir = tempdir().unwrap();
        let p = dir.path().join("fake_raw.bin");

        let tiny = build_jpeg(16, 12);
        let large = build_jpeg(320, 240);

        // Write TIFF with EXIF pointing at the tiny JPEG.
        write_fake_tiff_with_jpeg(&p, &tiny);

        // Append padding + the large preview JPEG (untagged).
        let mut body = std::fs::read(&p).unwrap();
        body.extend_from_slice(&[0xAAu8; 256]);
        body.extend_from_slice(&large);
        std::fs::write(&p, &body).unwrap();

        let t = extract_largest_preview_jpeg(&p, THUMB_MAX_DIM).expect("largest preview");
        // Long edge of large source is 320; after THUMB_MAX_DIM scale
        // we still expect something clearly bigger than the 16×12 thumb.
        assert!(
            t.width.max(t.height) >= 64,
            "expected large preview, got {}x{}",
            t.width,
            t.height
        );
    }

    #[test]
    fn non_jpeg_at_exif_offset_is_ignored() {
        // Synthesize a TIFF that points JPEGInterchangeFormat at non-JPEG
        // bytes (a fake "raw CFA" block). extract_embedded should refuse
        // to decode it and fall through to the scan path.
        let dir = tempdir().unwrap();
        let p = dir.path().join("fake_raw.bin");

        // 64 bytes of fake "thumbnail data" with random non-JPEG magic.
        let fake_thumb: Vec<u8> = (0..64).map(|i| (i as u8) ^ 0xAA).collect();

        write_fake_tiff_with_jpeg(&p, &fake_thumb);
        // We don't insert any real JPEG into the file body, so the scan
        // path won't find one either -- we just want extract_embedded to
        // skip this candidate without panicking.
        let _ = extract_embedded(&p);
    }
}
