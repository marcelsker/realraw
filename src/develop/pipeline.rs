//! Linear RAW develop + tone stage.
//!
//! Pipeline:
//! 1. LibRaw demosaic + camera WB + matrix + highlight recovery
//! 2. Linear sRGB f32 (`LinearPreview`, no gamma)
//! 3. Cache f32 RGB
//! 4. Tone: exposure → contrast → H/S/W/B → sRGB OETF → Rgba8

use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use libraw_sys as libraw;
use rawler::imgop::srgb::srgb_apply_gamma;

use super::decode::{DecodeError, PreviewImage, PreviewSource, PREVIEW_MAX_DIM};
use super::settings::ToneParams;

/// LibRaw `params.highlight`: 0=clip, 1=unclip, 2=blend, 3..=9=rebuild.
const LIBRAW_HIGHLIGHT_REBUILD: i32 = 3;
/// LibRaw linear + `no_auto_bright` lands ~1 EV dark vs camera / prior rawler path.
const LIBRAW_EV_COMP: f32 = 1.0;

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
/// downscaled so the longest edge is at most `max_dim` (use
/// [`super::decode::PREVIEW_MAX_DIM`] for interactive previews, or `u32::MAX`
/// for full-resolution export).
pub fn develop_linear(
    path: &Path,
    orientation: Option<i64>,
    max_dim: u32,
) -> Result<LinearPreview, DecodeError> {
    develop_linear_with_progress(path, orientation, max_dim, &mut |_| {})
}

/// Same as [`develop_linear`], reporting coarse stage progress via
/// `on_progress` (`0.0..=1.0`). Stages: decode → demosaic → orient → downscale.
pub fn develop_linear_with_progress(
    path: &Path,
    orientation: Option<i64>,
    max_dim: u32,
    on_progress: &mut dyn FnMut(f32),
) -> Result<LinearPreview, DecodeError> {
    if !super::decode::is_raw_path(path) {
        return Err(DecodeError::NotRaw);
    }
    on_progress(0.0);
    match panic::catch_unwind(AssertUnwindSafe(|| {
        develop_linear_inner(path, orientation, max_dim, on_progress)
    })) {
        Ok(result) => result,
        Err(_) => Err(DecodeError::Raw(
            "libraw linear develop panicked (unsupported or corrupt file)".into(),
        )),
    }
}

fn develop_linear_inner(
    path: &Path,
    orientation: Option<i64>,
    max_dim: u32,
    on_progress: &mut dyn FnMut(f32),
) -> Result<LinearPreview, DecodeError> {
    on_progress(0.05);
    let buf = std::fs::read(path)?;
    on_progress(0.10);

    let (mut width, mut height, mut rgb) = libraw_develop_linear(&buf, max_dim, on_progress)?;
    on_progress(0.72);

    let ori = orientation.unwrap_or(1);
    let (w2, h2, rgb2) = apply_orientation_rgb(rgb, width, height, ori);
    width = w2;
    height = h2;
    rgb = rgb2;
    on_progress(0.75);

    if width <= max_dim && height <= max_dim {
        on_progress(0.95);
        return Ok(LinearPreview {
            width,
            height,
            rgb,
        });
    }

    let (w3, h3, rgb3) =
        downscale_rgb_with_progress(&rgb, width, height, max_dim, on_progress, 0.75, 0.95);
    on_progress(0.95);
    Ok(LinearPreview {
        width: w3,
        height: h3,
        rgb: rgb3,
    })
}

/// LibRaw develop → linear sRGB f32 RGB (interleaved).
fn libraw_develop_linear(
    buf: &[u8],
    max_dim: u32,
    on_progress: &mut dyn FnMut(f32),
) -> Result<(u32, u32, Vec<f32>), DecodeError> {
    unsafe {
        let lr = libraw::libraw_init(0);
        if lr.is_null() {
            return Err(DecodeError::Raw("libraw_init failed".into()));
        }
        // Ensure cleanup on all paths.
        struct LibRawGuard(*mut libraw::libraw_data_t);
        impl Drop for LibRawGuard {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    unsafe { libraw::libraw_close(self.0) };
                }
            }
        }
        let _guard = LibRawGuard(lr);

        libraw_check(libraw::libraw_open_buffer(
            lr,
            buf.as_ptr() as *const _,
            buf.len() as _,
        ))?;

        // Params before unpack (half_size) / process (the rest).
        {
            let p = &mut (*lr).params;
            if max_dim <= PREVIEW_MAX_DIM {
                p.half_size = 1;
            }
            p.use_camera_wb = 1;
            p.use_camera_matrix = 1;
            p.highlight = LIBRAW_HIGHLIGHT_REBUILD;
            p.no_auto_bright = 1;
            // Fixed +1 EV: matches camera midtones without LibRaw auto-bright.
            p.bright = 2f32.powf(LIBRAW_EV_COMP);
            p.output_color = 1; // sRGB
            p.output_bps = 16;
            // Linear transfer so our tone stage owns the OETF.
            p.gamm[0] = 1.0;
            p.gamm[1] = 1.0;
            // Catalog / EXIF orientation applied after.
            p.user_flip = 0;
        }

        on_progress(0.15);
        libraw_check(libraw::libraw_unpack(lr))?;
        on_progress(0.40);
        libraw_check(libraw::libraw_dcraw_process(lr))?;
        on_progress(0.65);

        let mut err = 0i32;
        let mem = libraw::libraw_dcraw_make_mem_image(lr, &mut err);
        libraw_check(err)?;
        if mem.is_null() {
            return Err(DecodeError::Raw("libraw_dcraw_make_mem_image null".into()));
        }
        struct MemGuard(*mut libraw::libraw_processed_image_t);
        impl Drop for MemGuard {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    unsafe { libraw::libraw_dcraw_clear_mem(self.0) };
                }
            }
        }
        let mem = MemGuard(mem);

        let width = (*mem.0).width as u32;
        let height = (*mem.0).height as u32;
        let colors = (*mem.0).colors as usize;
        let bits = (*mem.0).bits as usize;
        if colors < 3 {
            return Err(DecodeError::Raw(format!(
                "libraw: expected ≥3 channels, got {colors}"
            )));
        }
        if bits != 16 {
            return Err(DecodeError::Raw(format!(
                "libraw: expected 16-bit, got {bits}"
            )));
        }

        let n = width as usize * height as usize;
        let data_size = (*mem.0).data_size as usize;
        let sample_count = data_size / 2;
        if sample_count < n * colors {
            return Err(DecodeError::Raw(format!(
                "libraw: buffer short ({sample_count} samples for {width}x{height}x{colors})"
            )));
        }
        let samples = std::slice::from_raw_parts((*mem.0).data.as_ptr() as *const u16, sample_count);

        let mut rgb = Vec::with_capacity(n * 3);
        let scale = 1.0 / 65535.0;
        for i in 0..n {
            let o = i * colors;
            rgb.push(samples[o] as f32 * scale);
            rgb.push(samples[o + 1] as f32 * scale);
            rgb.push(samples[o + 2] as f32 * scale);
        }

        // Guards drop here (mem image then libraw handle).
        Ok((width, height, rgb))
    }
}

fn libraw_check(code: i32) -> Result<(), DecodeError> {
    if code == 0 {
        return Ok(());
    }
    let msg = unsafe {
        let p = libraw::libraw_strerror(code);
        if p.is_null() {
            format!("libraw error {code}")
        } else {
            std::ffi::CStr::from_ptr(p)
                .to_string_lossy()
                .into_owned()
        }
    };
    Err(DecodeError::Raw(msg))
}

/// Apply exposure only. Prefer [`apply_tone`] when other light sliders are set.
pub fn apply_exposure(linear: &LinearPreview, exposure_ev: f32, max_dim: u32) -> PreviewImage {
    apply_tone(linear, &ToneParams::exposure_only(exposure_ev), max_dim)
}

/// Apply light-panel tone in linear light, then sRGB OETF → 8-bit RGBA.
///
/// Order: `L * 2^EV` → contrast → highlights/shadows → whites/blacks → clamp → OETF.
/// `max_dim` caps the longest edge (typically the on-screen preview size).
pub fn apply_tone(linear: &LinearPreview, tone: &ToneParams, max_dim: u32) -> PreviewImage {
    let (src_w, src_h, src) = if linear.width <= max_dim && linear.height <= max_dim {
        (linear.width, linear.height, linear.rgb.as_slice())
    } else {
        // Nearest-neighbor proxy from the full linear buffer.
        let (w, h, buf) = downscale_rgb_nearest(&linear.rgb, linear.width, linear.height, max_dim);
        return tone_to_preview(&buf, w, h, tone);
    };
    tone_to_preview(src, src_w, src_h, tone)
}

/// sRGB EOTF (encoded → linear). Inverse of [`srgb_apply_gamma`].
#[inline]
fn srgb_eotf(u: f32) -> f32 {
    if u <= 0.04045 {
        u / 12.92
    } else {
        ((u + 0.055) / 1.055).powf(2.4)
    }
}

#[inline]
fn sigmoid(v: f32) -> f32 {
    1.0 / (1.0 + (-v).exp())
}

/// Normalized sigmoid S-curve on `[0, 1]`: fixes black/white, holds mid-gray.
/// `k > 0` steeper midtones (more contrast). Unlike a linear pivot, endpoints
/// stay put so overall brightness does not collapse on dark frames.
#[inline]
fn s_curve(x: f32, k: f32) -> f32 {
    let a = sigmoid(k * (x - 0.5));
    let a0 = sigmoid(k * -0.5);
    let a1 = sigmoid(k * 0.5);
    ((a - a0) / (a1 - a0)).clamp(0.0, 1.0)
}

/// Contrast curve in sRGB-encoded luminance (`t` = slider/100).
///
/// - Positive: S-curve (fixed 0/1, mid-gray stable) — punch without global darkening
/// - Negative: mild linear flatten toward mid-gray — no gray fog
#[inline]
fn contrast_curve(x: f32, t: f32) -> f32 {
    if t >= 0.0 {
        // k: 1 ≈ gentle, ~3.2 at +100
        let k = 1.0 + 2.2 * t;
        let s = s_curve(x, k);
        x + t * (s - x)
    } else {
        let slope = 1.0 + 0.45 * t; // −100 → 0.55×
        (0.5 + (x - 0.5) * slope).clamp(0.0, 1.0)
    }
}

/// Soft weight for the lower end of the tone scale (shadows / blacks).
#[inline]
fn low_mask(x: f32, power: f32) -> f32 {
    (1.0 - x).clamp(0.0, 1.0).powf(power)
}

/// Soft weight for the upper end of the tone scale (highlights / whites).
#[inline]
fn high_mask(x: f32, power: f32) -> f32 {
    x.clamp(0.0, 1.0).powf(power)
}

/// Highlights / shadows / whites / blacks on sRGB-encoded luminance (`-100..=100`).
///
/// Regional masks: shadows/highlights are broader; blacks/whites hug the ends.
/// Positive shadows/blacks lift darks; positive highlights/whites lift brights.
/// Negative signs reverse (recover highlights, deepen shadows, etc.).
#[inline]
fn region_curve(x: f32, highlights: f32, shadows: f32, whites: f32, blacks: f32) -> f32 {
    let h = (highlights / 100.0).clamp(-1.0, 1.0);
    let s = (shadows / 100.0).clamp(-1.0, 1.0);
    let w = (whites / 100.0).clamp(-1.0, 1.0);
    let b = (blacks / 100.0).clamp(-1.0, 1.0);
    if h.abs() < 1e-6 && s.abs() < 1e-6 && w.abs() < 1e-6 && b.abs() < 1e-6 {
        return x;
    }

    let mut y = x;
    // Shadows first (broad dark lift/crush), then highlights.
    if s.abs() > 1e-6 {
        y = (y + s * 0.42 * low_mask(y, 2.0)).clamp(0.0, 1.0);
    }
    if h.abs() > 1e-6 {
        y = (y + h * 0.42 * high_mask(y, 2.0)).clamp(0.0, 1.0);
    }
    // Endpoint controls: narrower masks than H/S.
    if b.abs() > 1e-6 {
        y = (y + b * 0.32 * low_mask(y, 3.0)).clamp(0.0, 1.0);
    }
    if w.abs() > 1e-6 {
        y = (y + w * 0.32 * high_mask(y, 3.0)).clamp(0.0, 1.0);
    }
    y
}

/// Full light tone on linear RGB: luminance curves in sRGB space, rescale
/// channels to preserve chromaticity.
#[inline]
fn apply_tone_rgb(r: f32, g: f32, b: f32, tone: &ToneParams) -> [f32; 3] {
    let needs_curve = tone.contrast.abs() > 1e-6
        || tone.highlights.abs() > 1e-6
        || tone.shadows.abs() > 1e-6
        || tone.whites.abs() > 1e-6
        || tone.blacks.abs() > 1e-6;
    if !needs_curve {
        return [r, g, b];
    }
    // Rec.709 linear luminance
    let y = 0.212_672_9 * r + 0.715_152_2 * g + 0.072_175_0 * b;
    if y <= 1e-10 {
        return [r.max(0.0), g.max(0.0), b.max(0.0)];
    }

    let mut ye = srgb_apply_gamma(y.clamp(0.0, 1.0));
    let ct = (tone.contrast / 100.0).clamp(-1.0, 1.0);
    if ct.abs() > 1e-6 {
        ye = contrast_curve(ye, ct);
    }
    ye = region_curve(ye, tone.highlights, tone.shadows, tone.whites, tone.blacks);
    let y2 = srgb_eotf(ye);
    let scale = y2 / y;
    [(r * scale).max(0.0), (g * scale).max(0.0), (b * scale).max(0.0)]
}

fn tone_to_preview(rgb: &[f32], width: u32, height: u32, tone: &ToneParams) -> PreviewImage {
    let gain = 2f32.powf(tone.exposure);
    let n = width as usize * height as usize;
    let mut rgba = Vec::with_capacity(n * 4);
    for i in 0..n {
        let base = i * 3;
        let rl = rgb[base] * gain;
        let gl = rgb[base + 1] * gain;
        let bl = rgb[base + 2] * gain;
        let [rl, gl, bl] = apply_tone_rgb(rl, gl, bl, tone);
        let r = srgb_apply_gamma(rl.clamp(0.0, 1.0));
        let g = srgb_apply_gamma(gl.clamp(0.0, 1.0));
        let b = srgb_apply_gamma(bl.clamp(0.0, 1.0));
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
    use crate::develop::ToneParams;

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

    fn tone_contrast(c: f32) -> ToneParams {
        ToneParams {
            contrast: c,
            ..ToneParams::default()
        }
    }

    fn tone_with(mut f: impl FnMut(&mut ToneParams)) -> ToneParams {
        let mut t = ToneParams::default();
        f(&mut t);
        t
    }

    #[test]
    fn contrast_zero_matches_exposure_only() {
        let lin = solid(4, 4, 0.25);
        let a = apply_exposure(&lin, 0.5, 4);
        let b = apply_tone(&lin, &ToneParams::exposure_only(0.5), 4);
        assert_eq!(a.rgba, b.rgba);
    }

    #[test]
    fn positive_contrast_spreads_from_pivot() {
        // Below mid-gray: darkens; above: brightens.
        let dark = solid(2, 2, 0.05);
        let bright = solid(2, 2, 0.5);
        let d0 = apply_tone(&dark, &ToneParams::default(), 2);
        let d1 = apply_tone(&dark, &tone_contrast(80.0), 2);
        let b0 = apply_tone(&bright, &ToneParams::default(), 2);
        let b1 = apply_tone(&bright, &tone_contrast(80.0), 2);
        assert!(d1.rgba[0] < d0.rgba[0], "dark should darken: {} vs {}", d1.rgba[0], d0.rgba[0]);
        assert!(b1.rgba[0] > b0.rgba[0], "bright should brighten: {} vs {}", b1.rgba[0], b0.rgba[0]);
    }

    #[test]
    fn negative_contrast_compresses_toward_pivot() {
        let dark = solid(2, 2, 0.05);
        let bright = solid(2, 2, 0.5);
        let d0 = apply_tone(&dark, &ToneParams::default(), 2);
        let d1 = apply_tone(&dark, &tone_contrast(-80.0), 2);
        let b0 = apply_tone(&bright, &ToneParams::default(), 2);
        let b1 = apply_tone(&bright, &tone_contrast(-80.0), 2);
        assert!(d1.rgba[0] > d0.rgba[0], "dark should lift: {} vs {}", d1.rgba[0], d0.rgba[0]);
        assert!(b1.rgba[0] < b0.rgba[0], "bright should drop: {} vs {}", b1.rgba[0], b0.rgba[0]);
    }

    #[test]
    fn pivot_gray_stable_under_contrast() {
        // Linear value of sRGB mid-gray (OETF⁻¹(0.5)).
        let lin = solid(2, 2, 0.214_041_14);
        let a = apply_tone(&lin, &ToneParams::default(), 2);
        let b = apply_tone(&lin, &tone_contrast(100.0), 2);
        let c = apply_tone(&lin, &tone_contrast(-100.0), 2);
        assert_eq!(a.rgba[0], b.rgba[0]);
        assert_eq!(a.rgba[0], c.rgba[0]);
    }

    #[test]
    fn negative_contrast_preserves_chromaticity() {
        // Warm pixel: must not collapse toward neutral gray.
        let lin = LinearPreview {
            width: 1,
            height: 1,
            rgb: vec![0.6, 0.25, 0.08],
        };
        let img = apply_tone(&lin, &tone_contrast(-100.0), 1);
        let r = img.rgba[0] as i16;
        let g = img.rgba[1] as i16;
        let b = img.rgba[2] as i16;
        assert!(r > g + 20, "should stay warm: r={r} g={g} b={b}");
        assert!(g > b, "should stay warm: r={r} g={g} b={b}");
        // Must not wash to mid-gray fog (~128,128,128).
        let mean = (r + g + b) / 3;
        assert!(
            (r - mean).abs() > 15 || (g - mean).abs() > 15,
            "too neutral: r={r} g={g} b={b}"
        );
    }

    #[test]
    fn negative_contrast_keeps_shadow_depth() {
        // At −100, deep shadows must not lift to mid-gray.
        let dark = solid(2, 2, 0.02);
        let img = apply_tone(&dark, &tone_contrast(-100.0), 2);
        assert!(
            img.rgba[0] < 90,
            "shadows washed out: got {}",
            img.rgba[0]
        );
    }

    #[test]
    fn positive_contrast_does_not_crush_shadows() {
        // Linear mid-pivot with high slope crushed dark frames; S-curve must not.
        let dark = solid(2, 2, 0.08);
        let base = apply_tone(&dark, &ToneParams::default(), 2);
        let punch = apply_tone(&dark, &tone_contrast(100.0), 2);
        // May darken a little, but must stay well above near-black.
        assert!(
            punch.rgba[0] > 30,
            "shadows crushed: base={} punch={}",
            base.rgba[0],
            punch.rgba[0]
        );
        // And not a large global collapse (e.g. half the tone).
        assert!(
            punch.rgba[0] as i16 > base.rgba[0] as i16 / 2,
            "too dark overall: base={} punch={}",
            base.rgba[0],
            punch.rgba[0]
        );
    }

    #[test]
    fn positive_shadows_lift_darks() {
        let dark = solid(2, 2, 0.04);
        let base = apply_tone(&dark, &ToneParams::default(), 2);
        let lifted = apply_tone(
            &dark,
            &tone_with(|t| t.shadows = 80.0),
            2,
        );
        assert!(lifted.rgba[0] > base.rgba[0]);
    }

    #[test]
    fn negative_highlights_darken_brights() {
        let bright = solid(2, 2, 0.7);
        let base = apply_tone(&bright, &ToneParams::default(), 2);
        let recovered = apply_tone(
            &bright,
            &tone_with(|t| t.highlights = -80.0),
            2,
        );
        assert!(recovered.rgba[0] < base.rgba[0]);
    }

    #[test]
    fn whites_and_blacks_move_endpoints() {
        let dark = solid(2, 2, 0.03);
        let bright = solid(2, 2, 0.75);
        let d0 = apply_tone(&dark, &ToneParams::default(), 2);
        let d1 = apply_tone(&dark, &tone_with(|t| t.blacks = 80.0), 2);
        let b0 = apply_tone(&bright, &ToneParams::default(), 2);
        let b1 = apply_tone(&bright, &tone_with(|t| t.whites = -80.0), 2);
        assert!(d1.rgba[0] > d0.rgba[0], "blacks+ should lift darks");
        assert!(b1.rgba[0] < b0.rgba[0], "whites- should drop brights");
    }
}
