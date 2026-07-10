use std::io::Write;
use std::path::{Path, PathBuf};

use crate::import::thumbnail::Thumbnail;
use crate::thumb_grid::ThumbnailBytes;

const THUMBS_DIR: &str = "Thumbnails";
const FILES_PER_SHARD: i64 = 32;
const JPEG_QUALITY: u8 = 85;
/// Longest edge of cached library thumbnails. Matches
/// [`crate::import::thumbnail::THUMB_MAX_DIM`].
const CACHE_MAX_DIM: u32 = 1024;

/// Compute the shard folder index for a photo id.
fn shard_id(photo_id: i64) -> i64 {
    (photo_id - 1) / FILES_PER_SHARD
}

/// Compute the on-disk path for a cached thumbnail.
pub fn thumbnail_path(catalog_dir: &Path, photo_id: i64) -> PathBuf {
    catalog_dir
        .join(THUMBS_DIR)
        .join(shard_id(photo_id).to_string())
        .join(format!("{photo_id}.jpg"))
}

/// Load a cached thumbnail from disk. Returns `None` if the file
/// doesn't exist or can't be decoded.
pub fn load_thumbnail(catalog_dir: &Path, photo_id: i64) -> Option<ThumbnailBytes> {
    let path = thumbnail_path(catalog_dir, photo_id);
    if !path.exists() {
        return None;
    }
    let reader = image::ImageReader::open(&path).ok()?;
    let img = reader.decode().ok()?;
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    Some(ThumbnailBytes {
        width,
        height,
        rgba: rgba.into_raw(),
    })
}

/// Resize a Thumbnail so the longest edge is `max_dim`.
fn resize_thumbnail(thumb: &Thumbnail, max_dim: u32) -> Thumbnail {
    let img = match image::RgbaImage::from_raw(thumb.width, thumb.height, thumb.rgba.clone()) {
        Some(img) => img,
        None => return thumb.clone(),
    };
    let (w, h) = if thumb.width >= thumb.height {
        (max_dim, (thumb.height * max_dim / thumb.width.max(1)).max(1))
    } else {
        ((thumb.width * max_dim / thumb.height.max(1)).max(1), max_dim)
    };
    let small = image::imageops::resize(
        &img,
        w,
        h,
        image::imageops::FilterType::Triangle,
    );
    let (w2, h2) = small.dimensions();
    Thumbnail {
        width: w2,
        height: h2,
        rgba: small.into_raw(),
        max_dim,
    }
}

/// Save a thumbnail as JPEG to the disk cache
/// (`CACHE_MAX_DIM` long edge) with quality 85. Uses atomic write:
/// temp file → rename.
pub fn save_thumbnail(
    catalog_dir: &Path,
    photo_id: i64,
    thumb: &Thumbnail,
) -> Result<(), Box<dyn std::error::Error>> {
    let target = thumbnail_path(catalog_dir, photo_id);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let small = resize_thumbnail(thumb, CACHE_MAX_DIM);

    let temp_path = target.with_extension("tmp");
    {
        let file = std::fs::File::create(&temp_path)?;
        let mut writer = std::io::BufWriter::new(file);
        // JPEG doesn't support alpha: strip the channel before encoding.
        let img = image::RgbaImage::from_raw(small.width, small.height, small.rgba)
            .ok_or("invalid thumbnail dimensions")?;
        let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, JPEG_QUALITY);
        encoder.encode(
            &rgb,
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )?;
        writer.flush()?;
    }
    std::fs::rename(&temp_path, &target)?;
    Ok(())
}

/// Get a thumbnail from the disk cache, or generate and cache it.
///
/// 1. Check the disk cache (`Thumbnails/{shard}/{photo_id}.jpg`).
/// 2. On miss, extract from the source file via `extract_thumbnail`.
/// 3. Resize to `CACHE_MAX_DIM` and save as a JPEG to the disk cache.
/// 4. Return the cached-size result as `ThumbnailBytes`.
pub fn get_or_generate(
    catalog_dir: &Path,
    photo_id: i64,
    source_path: &Path,
) -> Result<ThumbnailBytes, String> {
    if let Some(bytes) = load_thumbnail(catalog_dir, photo_id) {
        return Ok(bytes);
    }

    let thumb = crate::import::thumbnail::extract_thumbnail(source_path)
        .map_err(|e| e.to_string())?;

    let small = resize_thumbnail(&thumb, CACHE_MAX_DIM);

    if let Err(e) = save_thumbnail(catalog_dir, photo_id, &thumb) {
        eprintln!("failed to cache thumbnail for photo {photo_id}: {e}");
    }

    Ok(ThumbnailBytes {
        width: small.width,
        height: small.height,
        rgba: small.rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_test_thumbnail(w: u32, h: u32) -> Thumbnail {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 128, 255]);
            }
        }
        Thumbnail {
            width: w,
            height: h,
            rgba,
            max_dim: 256,
        }
    }

    #[test]
    fn round_trip_save_and_load() {
        let dir = tempdir().unwrap();
        let thumb = make_test_thumbnail(256, 170);
        save_thumbnail(dir.path(), 1, &thumb).unwrap();

        let loaded = load_thumbnail(dir.path(), 1).expect("should load");
        assert!(loaded.width <= CACHE_MAX_DIM);
        assert!(loaded.height <= CACHE_MAX_DIM);
        assert!(!loaded.rgba.is_empty());
    }

    #[test]
    fn missing_returns_none() {
        let dir = tempdir().unwrap();
        assert!(load_thumbnail(dir.path(), 999).is_none());
    }

    #[test]
    fn get_or_generate_fresh_generates_and_caches() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("test.png");
        let img = image::RgbaImage::from_fn(400, 300, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        });
        img.save(&src).unwrap();

        let bytes = get_or_generate(dir.path(), 42, &src).expect("should generate");
        assert!(bytes.width <= CACHE_MAX_DIM);
        assert!(bytes.height <= CACHE_MAX_DIM);
        assert!(!bytes.rgba.is_empty());

        // Second call should load from cache
        let cached = get_or_generate(dir.path(), 42, &src).expect("should load from cache");
        assert_eq!(bytes.width, cached.width);
        assert_eq!(bytes.height, cached.height);
    }

    #[test]
    fn thumbnail_path_uses_shards() {
        let d = Path::new("/base");
        assert!(thumbnail_path(d, 1).starts_with(d.join("Thumbnails/0/")));
        assert!(thumbnail_path(d, 32).starts_with(d.join("Thumbnails/0/")));
        assert!(thumbnail_path(d, 33).starts_with(d.join("Thumbnails/1/")));
        assert_eq!(
            thumbnail_path(d, 1).file_name().unwrap(),
            "1.jpg",
        );
    }
}
