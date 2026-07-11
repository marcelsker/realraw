//! On-disk demosaic preview cache for Develop mode.
//!
//! Under `{catalog_dir}/Previews/`:
//!
//! * **JPEG placeholder** (`cacache-sync` key `demosaic/{id}`) — sRGB
//!   EV=0 paint for the first frame after open.
//! * **Linear demosaic** (`linear/{shard}/{id}.rrln`) — f32 RGB buffer
//!   so reopening a photo **skips** rawler demosaic entirely.
//!
//! Both are at develop preview resolution ([`crate::develop::PREVIEW_MAX_DIM`]).

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use crate::develop::{LinearPreview, PreviewImage, PreviewSource, PREVIEW_MAX_DIM};

const PREVIEWS_DIR: &str = "Previews";
const LINEAR_DIR: &str = "linear";
const JPEG_QUALITY: u8 = 92;
const FILES_PER_SHARD: i64 = 32;

/// Magic + version for `.rrln` linear cache files.
const RRLN_MAGIC: &[u8; 4] = b"RRLN";
/// Bump when linear buffer semantics change (e.g. highlight reconstruction).
const RRLN_VERSION: u32 = 9;

/// Root directory of the demosaic preview cache.
pub fn cache_dir(catalog_dir: &Path) -> PathBuf {
    catalog_dir.join(PREVIEWS_DIR)
}

fn cache_key(photo_id: i64) -> String {
    format!("demosaic/{photo_id}")
}

fn shard_id(photo_id: i64) -> i64 {
    (photo_id.max(1) - 1) / FILES_PER_SHARD
}

/// Path of the linear demosaic buffer for `photo_id`.
pub fn linear_path(catalog_dir: &Path, photo_id: i64) -> PathBuf {
    cache_dir(catalog_dir)
        .join(LINEAR_DIR)
        .join(shard_id(photo_id).to_string())
        .join(format!("{photo_id}.rrln"))
}

/// Load a cached demosaic preview. Returns `None` on miss or corruption
/// (cacache verifies integrity; bad entries are treated as misses).
pub fn load_preview(catalog_dir: &Path, photo_id: i64) -> Option<PreviewImage> {
    let dir = cache_dir(catalog_dir);
    let key = cache_key(photo_id);
    let data = cacache_sync::read(&dir, &key).ok()?;
    decode_jpeg_preview(&data)
}

/// Encode `img` as JPEG and store it under `photo_id`.
///
/// Only call for final develop images (demosaic / decoder preview).
pub fn save_preview(
    catalog_dir: &Path,
    photo_id: i64,
    img: &PreviewImage,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let jpeg = encode_jpeg_preview(img)?;
    let dir = cache_dir(catalog_dir);
    let key = cache_key(photo_id);
    cacache_sync::write(&dir, &key, &jpeg)?;
    Ok(())
}

/// Load a cached linear demosaic buffer, if valid for `orientation` and
/// current [`PREVIEW_MAX_DIM`].
pub fn load_linear(
    catalog_dir: &Path,
    photo_id: i64,
    orientation: Option<i64>,
) -> Option<LinearPreview> {
    let path = linear_path(catalog_dir, photo_id);
    let mut f = std::fs::File::open(&path).ok()?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).ok()?;
    if &magic != RRLN_MAGIC {
        return None;
    }
    let mut hdr = [0u8; 4 + 4 + 4 + 8 + 4]; // ver, w, h, ori, max_dim
    f.read_exact(&mut hdr).ok()?;
    let version = u32::from_le_bytes(hdr[0..4].try_into().ok()?);
    if version != RRLN_VERSION {
        return None;
    }
    let width = u32::from_le_bytes(hdr[4..8].try_into().ok()?);
    let height = u32::from_le_bytes(hdr[8..12].try_into().ok()?);
    let stored_ori = i64::from_le_bytes(hdr[12..20].try_into().ok()?);
    let max_dim = u32::from_le_bytes(hdr[20..24].try_into().ok()?);

    let want_ori = orientation.unwrap_or(0);
    if stored_ori != want_ori || max_dim != PREVIEW_MAX_DIM {
        return None;
    }
    if width == 0 || height == 0 || width > PREVIEW_MAX_DIM * 2 || height > PREVIEW_MAX_DIM * 2 {
        return None;
    }

    let n = width as usize * height as usize * 3;
    let mut bytes = vec![0u8; n * 4];
    f.read_exact(&mut bytes).ok()?;
    // Reject trailing garbage / truncated files only if we can seek;
    // exact read is enough for validity.
    let mut rgb = Vec::with_capacity(n);
    for chunk in bytes.chunks_exact(4) {
        rgb.push(f32::from_le_bytes(chunk.try_into().ok()?));
    }
    Some(LinearPreview {
        width,
        height,
        rgb,
    })
}

/// Persist a linear demosaic buffer for fast reopen (skips rawler).
pub fn save_linear(
    catalog_dir: &Path,
    photo_id: i64,
    orientation: Option<i64>,
    linear: &LinearPreview,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let target = linear_path(catalog_dir, photo_id);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension("rrln.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(RRLN_MAGIC)?;
        f.write_all(&RRLN_VERSION.to_le_bytes())?;
        f.write_all(&linear.width.to_le_bytes())?;
        f.write_all(&linear.height.to_le_bytes())?;
        f.write_all(&orientation.unwrap_or(0).to_le_bytes())?;
        f.write_all(&PREVIEW_MAX_DIM.to_le_bytes())?;
        // One bulk write — per-float write_all was millions of syscalls.
        let mut bytes = Vec::with_capacity(linear.rgb.len() * 4);
        for &v in &linear.rgb {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &target)?;
    Ok(())
}

/// Drop JPEG + linear cache entries for `photo_id`.
pub fn remove_preview(catalog_dir: &Path, photo_id: i64) {
    let dir = cache_dir(catalog_dir);
    let key = cache_key(photo_id);
    let _ = cacache_sync::remove(&dir, &key);
    let linear = linear_path(catalog_dir, photo_id);
    let _ = std::fs::remove_file(linear);
}

fn encode_jpeg_preview(
    img: &PreviewImage,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let rgba = image::RgbaImage::from_raw(img.width, img.height, img.rgba.clone())
        .ok_or("invalid preview dimensions")?;
    let rgb = image::DynamicImage::ImageRgba8(rgba).to_rgb8();
    let mut buf = Vec::new();
    let mut encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
    encoder.encode(
        &rgb,
        rgb.width(),
        rgb.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(buf)
}

fn decode_jpeg_preview(data: &[u8]) -> Option<PreviewImage> {
    let reader = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let img = reader.decode().ok()?;
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    Some(PreviewImage {
        width,
        height,
        rgba: rgba.into_raw(),
        source: PreviewSource::CachedPreview,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_image(w: u32, h: u32) -> PreviewImage {
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let x = (i % w) as u8;
                let y = (i / w) as u8;
                [x, y, 64, 255]
            })
            .collect();
        PreviewImage {
            width: w,
            height: h,
            rgba,
            source: PreviewSource::Demosaic,
        }
    }

    #[test]
    fn roundtrip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let img = sample_image(64, 48);
        save_preview(dir.path(), 7, &img).expect("save");
        let loaded = load_preview(dir.path(), 7).expect("load");
        assert_eq!(loaded.width, 64);
        assert_eq!(loaded.height, 48);
        assert_eq!(loaded.source, PreviewSource::CachedPreview);
        assert_eq!(loaded.rgba.len(), 64 * 48 * 4);
    }

    #[test]
    fn miss_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_preview(dir.path(), 99).is_none());
    }

    #[test]
    fn remove_drops_entry() {
        let dir = tempfile::tempdir().unwrap();
        let img = sample_image(16, 16);
        save_preview(dir.path(), 3, &img).unwrap();
        assert!(load_preview(dir.path(), 3).is_some());
        remove_preview(dir.path(), 3);
        assert!(load_preview(dir.path(), 3).is_none());
    }

    #[test]
    fn overwrite_replaces() {
        let dir = tempfile::tempdir().unwrap();
        save_preview(dir.path(), 1, &sample_image(8, 8)).unwrap();
        save_preview(dir.path(), 1, &sample_image(32, 16)).unwrap();
        let loaded = load_preview(dir.path(), 1).unwrap();
        assert_eq!((loaded.width, loaded.height), (32, 16));
    }

    fn sample_linear(w: u32, h: u32) -> LinearPreview {
        let n = (w * h * 3) as usize;
        let rgb: Vec<f32> = (0..n).map(|i| (i % 256) as f32 / 255.0).collect();
        LinearPreview {
            width: w,
            height: h,
            rgb,
        }
    }

    #[test]
    fn linear_roundtrip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let lin = sample_linear(32, 24);
        save_linear(dir.path(), 42, Some(1), &lin).unwrap();
        let loaded = load_linear(dir.path(), 42, Some(1)).expect("load linear");
        assert_eq!(loaded.width, 32);
        assert_eq!(loaded.height, 24);
        assert_eq!(loaded.rgb.len(), lin.rgb.len());
        assert!((loaded.rgb[0] - lin.rgb[0]).abs() < 1e-6);
        assert!((loaded.rgb[10] - lin.rgb[10]).abs() < 1e-6);
    }

    #[test]
    fn linear_miss_on_orientation_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        save_linear(dir.path(), 1, Some(1), &sample_linear(8, 8)).unwrap();
        assert!(load_linear(dir.path(), 1, Some(6)).is_none());
        assert!(load_linear(dir.path(), 1, Some(1)).is_some());
    }

    #[test]
    fn remove_drops_linear_too() {
        let dir = tempfile::tempdir().unwrap();
        save_linear(dir.path(), 9, None, &sample_linear(4, 4)).unwrap();
        save_preview(dir.path(), 9, &sample_image(4, 4)).unwrap();
        assert!(load_linear(dir.path(), 9, None).is_some());
        remove_preview(dir.path(), 9);
        assert!(load_linear(dir.path(), 9, None).is_none());
        assert!(load_preview(dir.path(), 9).is_none());
    }
}
