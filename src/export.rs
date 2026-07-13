//! Simple export: develop current photo + write JPEG/PNG.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::develop::{apply_tone, develop_linear, PreviewImage, ToneParams};
use crate::gpu::GpuContext;
use crate::task::{Task, TaskContext, TaskId, TaskManager};

/// JPEG quality for exported files.
const EXPORT_JPEG_QUALITY: u8 = 92;

/// Spawn a background task that develops `source` with tone params and
/// writes the result to `dest` (format from extension: `.png` or JPEG).
///
/// When `gpu` is `Some`, the tone stage runs on the GPU. Falls back to
/// CPU if wgpu init failed at startup.
pub fn spawn_export_task(
    mgr: &mut TaskManager,
    source: PathBuf,
    orientation: Option<i64>,
    tone: ToneParams,
    dest: PathBuf,
    gpu: Option<Arc<GpuContext>>,
) -> TaskId {
    let label = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".into());

    let task = Task::new(format!("Export {label}"), "Develop and save image").work(
        move |ctx: &TaskContext| {
            if ctx.is_cancelled() {
                return Err("cancelled".into());
            }
            ctx.set_message("Developing…");
            ctx.set_progress(0.1);

            // Full resolution — do not use PREVIEW_MAX_DIM.
            let linear =
                develop_linear(&source, orientation, u32::MAX).map_err(|e| e.to_string())?;
            if ctx.is_cancelled() {
                return Err("cancelled".into());
            }
            ctx.set_progress(0.65);
            ctx.set_message("Applying tone…");

            let img = if let Some(gpu) = gpu {
                let mut backend = crate::gpu::GpuBackend::new(gpu);
                backend
                    .apply(&linear, &tone, linear.width, linear.height)
                    .image
            } else {
                apply_tone(&linear, &tone, u32::MAX)
            };
            if ctx.is_cancelled() {
                return Err("cancelled".into());
            }
            ctx.set_progress(0.85);
            ctx.set_message("Writing…");

            write_image(&img, &dest)?;
            ctx.set_progress(1.0);
            ctx.set_message("Done");
            Ok(())
        },
    );

    let tid = mgr.add_task(task);
    mgr.start();
    tid
}

/// Ensure `path` has a known image extension; default to `.jpg`.
pub fn ensure_export_extension(path: PathBuf) -> PathBuf {
    let has_known = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png"
            )
        });
    if has_known {
        path
    } else {
        path.with_extension("jpg")
    }
}

fn write_image(img: &PreviewImage, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let rgba = image::RgbaImage::from_raw(img.width, img.height, img.rgba.clone())
        .ok_or_else(|| "invalid image dimensions".to_string())?;
    let dyn_img = image::DynamicImage::ImageRgba8(rgba);

    let ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jpg")
        .to_ascii_lowercase();

    match ext.as_str() {
        "png" => dyn_img
            .save_with_format(dest, image::ImageFormat::Png)
            .map_err(|e| e.to_string()),
        _ => {
            // JPEG: strip alpha.
            let rgb = dyn_img.to_rgb8();
            let file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
            let mut writer = std::io::BufWriter::new(file);
            let mut encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, EXPORT_JPEG_QUALITY);
            encoder
                .encode(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .map_err(|e| e.to_string())?;
            use std::io::Write;
            writer.flush().map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::develop::PreviewSource;

    #[test]
    fn ensure_extension_adds_jpg() {
        let p = ensure_export_extension(PathBuf::from("out"));
        assert_eq!(p.extension().and_then(|e| e.to_str()), Some("jpg"));
    }

    #[test]
    fn ensure_extension_keeps_png() {
        let p = ensure_export_extension(PathBuf::from("out.PNG"));
        assert_eq!(
            p.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()),
            Some("png".into())
        );
    }

    #[test]
    fn write_jpeg_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("t.jpg");
        let img = PreviewImage {
            width: 4,
            height: 4,
            rgba: vec![128; 4 * 4 * 4],
            source: PreviewSource::Demosaic,
        };
        write_image(&img, &dest).unwrap();
        assert!(dest.metadata().unwrap().len() > 0);
    }
}
