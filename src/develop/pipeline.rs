//! Linear RAW develop + exposure tone stage.
//!
//! Pipeline:
//! 1. rawler demosaic + WB + cam→linear sRGB (no gamma)
//! 2. Cache f32 RGB (`LinearPreview`)
//! 3. Exposure: `L * 2^EV`, then sRGB OETF → Rgba8

use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};
use rawler::imgop::srgb::srgb_apply_gamma;

use super::decode::{DecodeError, PreviewImage, PreviewSource, PREVIEW_MAX_DIM};

/// Linear (pre-gamma) develop buffer for interactive tone ops.
#[derive(Debug, Clone)]
pub struct LinearPreview {
    pub width: u32,
    pub height: u32,
    /// Interleaved RGB, length `width * height * 3`, scene-linear-ish.
    pub rgb: Vec<f32>,
}

impl LinearPreview {
    pub fn pixel_count(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

/// Demosaic + WB + calibrate, **without** sRGB gamma. Oriented and
/// downscaled to [`PREVIEW_MAX_DIM`].
pub fn develop_linear(
    path: &Path,
    orientation: Option<i64>,
) -> Result<LinearPreview, DecodeError> {
    develop_linear_with_progress(path, orientation, &mut |_| {})
}

/// Same as [`develop_linear`], reporting coarse stage progress via
/// `on_progress` (`0.0..=1.0`). Stages: decode → demosaic → orient → downscale.
pub fn develop_linear_with_progress(
    path: &Path,
    orientation: Option<i64>,
    on_progress: &mut dyn FnMut(f32),
) -> Result<LinearPreview, DecodeError> {
    if !super::decode::is_raw_path(path) {
        return Err(DecodeError::NotRaw);
    }
    on_progress(0.0);
    match panic::catch_unwind(AssertUnwindSafe(|| {
        develop_linear_inner(path, orientation, on_progress)
    })) {
        Ok(result) => result,
        Err(_) => Err(DecodeError::Raw(
            "rawler linear develop panicked (unsupported or corrupt file)".into(),
        )),
    }
}

fn develop_linear_inner(
    path: &Path,
    orientation: Option<i64>,
    on_progress: &mut dyn FnMut(f32),
) -> Result<LinearPreview, DecodeError> {
    on_progress(0.05);
    let raw = rawler::decode_file(path).map_err(|e| DecodeError::Raw(e.to_string()))?;
    on_progress(0.15);

    let ori = orientation.or_else(|| {
        let u = raw.orientation.to_u16();
        if u == 0 {
            None
        } else {
            Some(u as i64)
        }
    });

    let dev = RawDevelop {
        steps: vec![
            ProcessingStep::Rescale,
            ProcessingStep::Demosaic,
            ProcessingStep::CropActiveArea,
            ProcessingStep::WhiteBalance,
            ProcessingStep::Calibrate,
            ProcessingStep::CropDefault,
            // No ProcessingStep::SRgb — keep linear for exposure.
        ],
    };

    // Bulk of the work (rawler demosaic + WB + calibrate).
    // Cap at ~0.80 so the worker can still report cache/tone work after.
    on_progress(0.2);
    let intermediate = dev
        .develop_intermediate(&raw)
        .map_err(|e| DecodeError::Raw(e.to_string()))?;
    on_progress(0.65);

    let (mut width, mut height, mut rgb) = intermediate_to_rgb_f32(intermediate)?;
    on_progress(0.68);
    let (w2, h2, rgb2) = apply_orientation_rgb(rgb, width, height, ori.unwrap_or(1));
    width = w2;
    height = h2;
    rgb = rgb2;
    on_progress(0.72);

    // Box-filter downsample is CPU-heavy on full-res demosaic; report rows.
    let (w3, h3, rgb3) =
        downscale_rgb_with_progress(&rgb, width, height, PREVIEW_MAX_DIM, on_progress, 0.72, 0.95);
    on_progress(0.95);
    Ok(LinearPreview {
        width: w3,
        height: h3,
        rgb: rgb3,
    })
}

fn intermediate_to_rgb_f32(
    intermediate: Intermediate,
) -> Result<(u32, u32, Vec<f32>), DecodeError> {
    match intermediate {
        Intermediate::ThreeColor(pixels) => {
            let w = pixels.width as u32;
            let h = pixels.height as u32;
            let mut rgb = Vec::with_capacity(pixels.data.len() * 3);
            for p in &pixels.data {
                rgb.push(p[0]);
                rgb.push(p[1]);
                rgb.push(p[2]);
            }
            Ok((w, h, rgb))
        }
        Intermediate::FourColor(pixels) => {
            let w = pixels.width as u32;
            let h = pixels.height as u32;
            let mut rgb = Vec::with_capacity(pixels.data.len() * 3);
            for p in &pixels.data {
                rgb.push(p[0]);
                rgb.push(p[1]);
                rgb.push(p[2]);
            }
            Ok((w, h, rgb))
        }
        Intermediate::Monochrome(pixels) => {
            let w = pixels.dim().w as u32;
            let h = pixels.dim().h as u32;
            let mut rgb = Vec::with_capacity(pixels.data.len() * 3);
            for &v in &pixels.data {
                rgb.push(v);
                rgb.push(v);
                rgb.push(v);
            }
            Ok((w, h, rgb))
        }
    }
}

/// Apply exposure in linear light, then sRGB OETF → 8-bit RGBA.
///
/// `max_dim` caps the longest edge (typically the on-screen preview size).
pub fn apply_exposure(linear: &LinearPreview, exposure_ev: f32, max_dim: u32) -> PreviewImage {
    let (src_w, src_h, src) = if linear.width <= max_dim && linear.height <= max_dim {
        (linear.width, linear.height, linear.rgb.as_slice())
    } else {
        // Nearest-neighbor proxy from the full linear buffer.
        let (w, h, buf) = downscale_rgb_nearest(&linear.rgb, linear.width, linear.height, max_dim);
        return tone_to_preview(&buf, w, h, exposure_ev);
    };
    tone_to_preview(src, src_w, src_h, exposure_ev)
}

fn tone_to_preview(rgb: &[f32], width: u32, height: u32, exposure_ev: f32) -> PreviewImage {
    let gain = 2f32.powf(exposure_ev);
    let n = width as usize * height as usize;
    let mut rgba = Vec::with_capacity(n * 4);
    for i in 0..n {
        let base = i * 3;
        let r = srgb_apply_gamma((rgb[base] * gain).clamp(0.0, 1.0));
        let g = srgb_apply_gamma((rgb[base + 1] * gain).clamp(0.0, 1.0));
        let b = srgb_apply_gamma((rgb[base + 2] * gain).clamp(0.0, 1.0));
        rgba.push((r * 255.0 + 0.5) as u8);
        rgba.push((g * 255.0 + 0.5) as u8);
        rgba.push((b * 255.0 + 0.5) as u8);
        rgba.push(255);
    }
    PreviewImage {
        width,
        height,
        rgba,
        source: PreviewSource::Demosaic,
    }
}

fn apply_orientation_rgb(
    rgb: Vec<f32>,
    width: u32,
    height: u32,
    orientation: i64,
) -> (u32, u32, Vec<f32>) {
    let get = |rgb: &[f32], w: u32, x: u32, y: u32| -> [f32; 3] {
        let i = ((y * w + x) * 3) as usize;
        [rgb[i], rgb[i + 1], rgb[i + 2]]
    };
    let put = |out: &mut [f32], w: u32, x: u32, y: u32, p: [f32; 3]| {
        let i = ((y * w + x) * 3) as usize;
        out[i] = p[0];
        out[i + 1] = p[1];
        out[i + 2] = p[2];
    };

    match orientation {
        1 | 0 => (width, height, rgb),
        2 => {
            // flip H
            let mut out = vec![0.0; rgb.len()];
            for y in 0..height {
                for x in 0..width {
                    put(&mut out, width, width - 1 - x, y, get(&rgb, width, x, y));
                }
            }
            (width, height, out)
        }
        3 => {
            // 180
            let mut out = vec![0.0; rgb.len()];
            for y in 0..height {
                for x in 0..width {
                    put(
                        &mut out,
                        width,
                        width - 1 - x,
                        height - 1 - y,
                        get(&rgb, width, x, y),
                    );
                }
            }
            (width, height, out)
        }
        4 => {
            // flip V
            let mut out = vec![0.0; rgb.len()];
            for y in 0..height {
                for x in 0..width {
                    put(&mut out, width, x, height - 1 - y, get(&rgb, width, x, y));
                }
            }
            (width, height, out)
        }
        5 => {
            // transpose + flip H ≡ rotate 90 CW then flip H… EXIF 5: transpose
            // EXIF 5 = mirror horizontal then rotate 270 CW
            // Implement as: (x,y) -> (y, w-1-x) with new size h×w
            let (nw, nh) = (height, width);
            let mut out = vec![0.0; rgb.len()];
            for y in 0..height {
                for x in 0..width {
                    put(&mut out, nw, y, width - 1 - x, get(&rgb, width, x, y));
                }
            }
            (nw, nh, out)
        }
        6 => {
            // rotate 90 CW: (x,y) -> (h-1-y, x), size h×w
            let (nw, nh) = (height, width);
            let mut out = vec![0.0; rgb.len()];
            for y in 0..height {
                for x in 0..width {
                    put(&mut out, nw, height - 1 - y, x, get(&rgb, width, x, y));
                }
            }
            (nw, nh, out)
        }
        7 => {
            // EXIF 7: mirror horizontal then rotate 90 CW
            // (x,y) -> (h-1-y, w-1-x)
            let (nw, nh) = (height, width);
            let mut out = vec![0.0; rgb.len()];
            for y in 0..height {
                for x in 0..width {
                    put(
                        &mut out,
                        nw,
                        height - 1 - y,
                        width - 1 - x,
                        get(&rgb, width, x, y),
                    );
                }
            }
            (nw, nh, out)
        }
        8 => {
            // rotate 270 CW / 90 CCW: (x,y) -> (y, w-1-x)
            let (nw, nh) = (height, width);
            let mut out = vec![0.0; rgb.len()];
            for y in 0..height {
                for x in 0..width {
                    put(&mut out, nw, y, width - 1 - x, get(&rgb, width, x, y));
                }
            }
            (nw, nh, out)
        }
        _ => (width, height, rgb),
    }
}

/// Box-filter downsample; maps row progress into `[p0, p1]` via `on_progress`.
fn downscale_rgb_with_progress(
    rgb: &[f32],
    width: u32,
    height: u32,
    max_dim: u32,
    on_progress: &mut dyn FnMut(f32),
    p0: f32,
    p1: f32,
) -> (u32, u32, Vec<f32>) {
    if width <= max_dim && height <= max_dim {
        return (width, height, rgb.to_vec());
    }
    let scale = (max_dim as f32 / width.max(height) as f32).min(1.0);
    let nw = ((width as f32 * scale).round() as u32).max(1);
    let nh = ((height as f32 * scale).round() as u32).max(1);
    let mut out = vec![0.0f32; (nw * nh * 3) as usize];
    // Report every ~2% of output rows to keep the bar moving without flooding.
    let report_every = (nh / 50).max(1);
    for y in 0..nh {
        let y0 = (y as u64 * height as u64 / nh as u64) as u32;
        let y1 = (((y as u64 + 1) * height as u64 / nh as u64) as u32).max(y0 + 1);
        for x in 0..nw {
            let x0 = (x as u64 * width as u64 / nw as u64) as u32;
            let x1 = (((x as u64 + 1) * width as u64 / nw as u64) as u32).max(x0 + 1);
            let mut acc = [0.0f32; 3];
            let mut count = 0.0f32;
            for sy in y0..y1.min(height) {
                for sx in x0..x1.min(width) {
                    let i = ((sy * width + sx) * 3) as usize;
                    acc[0] += rgb[i];
                    acc[1] += rgb[i + 1];
                    acc[2] += rgb[i + 2];
                    count += 1.0;
                }
            }
            let o = ((y * nw + x) * 3) as usize;
            if count > 0.0 {
                out[o] = acc[0] / count;
                out[o + 1] = acc[1] / count;
                out[o + 2] = acc[2] / count;
            }
        }
        if y % report_every == 0 || y + 1 == nh {
            let t = (y + 1) as f32 / nh as f32;
            on_progress(p0 + (p1 - p0) * t);
        }
    }
    (nw, nh, out)
}

fn downscale_rgb_nearest(
    rgb: &[f32],
    width: u32,
    height: u32,
    max_dim: u32,
) -> (u32, u32, Vec<f32>) {
    if width <= max_dim && height <= max_dim {
        return (width, height, rgb.to_vec());
    }
    let scale = (max_dim as f32 / width.max(height) as f32).min(1.0);
    let nw = ((width as f32 * scale).round() as u32).max(1);
    let nh = ((height as f32 * scale).round() as u32).max(1);
    let mut out = vec![0.0f32; (nw * nh * 3) as usize];
    for y in 0..nh {
        let sy = (y as u64 * height as u64 / nh as u64) as u32;
        for x in 0..nw {
            let sx = (x as u64 * width as u64 / nw as u64) as u32;
            let i = ((sy * width + sx) * 3) as usize;
            let o = ((y * nw + x) * 3) as usize;
            out[o] = rgb[i];
            out[o + 1] = rgb[i + 1];
            out[o + 2] = rgb[i + 2];
        }
    }
    (nw, nh, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, v: f32) -> LinearPreview {
        LinearPreview {
            width: w,
            height: h,
            rgb: vec![v; (w * h * 3) as usize],
        }
    }

    #[test]
    fn exposure_zero_is_mid_gray() {
        let lin = solid(4, 4, 0.5);
        let img = apply_exposure(&lin, 0.0, 4);
        assert_eq!(img.width, 4);
        // sRGB gamma of 0.5 ≈ 188
        let r = img.rgba[0];
        assert!((180..=195).contains(&r), "got {r}");
    }

    #[test]
    fn plus_one_stop_brightens() {
        let lin = solid(2, 2, 0.25);
        let a = apply_exposure(&lin, 0.0, 2);
        let b = apply_exposure(&lin, 1.0, 2);
        assert!(b.rgba[0] > a.rgba[0]);
    }

    #[test]
    fn max_dim_caps_output() {
        let lin = solid(2000, 1000, 0.3);
        let img = apply_exposure(&lin, 0.0, 800);
        assert!(img.width <= 800);
        assert!(img.height <= 800);
    }
}
