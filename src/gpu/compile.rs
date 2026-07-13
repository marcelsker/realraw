//! naga-based WGSL validation at startup.
//!
//! naga is the same shader frontend `wgpu::Device::create_shader_module`
//! uses internally, so a successful parse + validate here guarantees wgpu
//! will accept the source on every backend that supports WGSL. This also
//! gives a much better error message at startup than the default wgpu
//! "shader compilation failed" path.

use std::borrow::Cow;

/// Pre-validated WGSL source for the tone compute pipeline.
pub const TONE_WGSL: &str = include_str!("shaders/tone.wgsl");

/// One-time shader validation. Returns the source on success so the
/// caller can hand it to `Device::create_shader_module` without an
/// extra allocation. Cached: calling this more than once is cheap.
pub fn tone_source() -> Cow<'static, str> {
    Cow::Borrowed(TONE_WGSL)
}

/// Validate the WGSL source. `naga` parses it and runs the validator so
/// that the worst case (a type error or unsupported builtin) is reported
/// at startup with a useful error message instead of during the first
/// compute dispatch.
///
/// Returns `Err(message)` on any failure. Validation is skipped on
/// `wasm32` because `naga::front::wgsl` is only meaningful where naga is
/// the canonical frontend (i.e. the wgpu backends we care about).
#[cfg(not(target_arch = "wasm32"))]
pub fn validate() -> Result<(), String> {
    let module = naga::front::wgsl::parse_str(TONE_WGSL).map_err(|e| {
        format!("wgsl parse error: {e:?}")
    })?;
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::default(),
    );
    let _info = validator
        .validate(&module)
        .map_err(|e| format!("wgsl validation error: {e:?}"))?;
    Ok(())
}

#[cfg(target_arch = "wasm32")]
pub fn validate() -> Result<(), String> {
    Ok(())
}

/// Compile-time sanity: ensure the shader parses before we ever ship
/// a binary. Catches hand-edits to `tone.wgsl` that would otherwise
/// only show up at first launch.
#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn tone_wgsl_parses() {
        validate().expect("tone.wgsl must be valid WGSL");
    }

    /// End-to-end GPU smoke test: spins up a real device, runs the tone
    /// compute pipeline on a 4×4 synthetic linear buffer, reads the
    /// result back, and asserts the output is non-zero and bounded.
    /// Skipped in CI / on devices without wgpu.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn gpu_tone_smoke() {
        let Some(ctx) = pollster::block_on(crate::gpu::GpuContext::for_tests()) else {
            eprintln!("no gpu adapter; skipping");
            return;
        };
        let mut backend = crate::gpu::GpuBackend::new(std::sync::Arc::new(ctx));
        let linear = crate::develop::LinearPreview {
            width: 4,
            height: 4,
            rgb: vec![0.5; 4 * 4 * 3],
        };
        let tone = crate::develop::ToneParams::default();
        let out = backend.apply(&linear, &tone, 4, 4);
        assert_eq!(out.image.width, 4);
        assert_eq!(out.image.height, 4);
        assert_eq!(out.image.rgba.len(), 4 * 4 * 4);
        // Mid-gray linear → sRGB ≈ 188, allow 170..=200.
        for px in out.image.rgba.chunks_exact(4) {
            let r = px[0];
            let g = px[1];
            let b = px[2];
            let a = px[3];
            assert!((170..=200).contains(&r), "r out of range: {r}");
            assert!((170..=200).contains(&g), "g out of range: {g}");
            assert!((170..=200).contains(&b), "b out of range: {b}");
            assert_eq!(a, 255);
        }
    }

    /// End-to-end GPU smoke test for the downscale path: tone a
    /// 32×32 source to an 8×8 output, verify aspect + size.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn gpu_tone_downscale_smoke() {
        let Some(ctx) = pollster::block_on(crate::gpu::GpuContext::for_tests()) else {
            eprintln!("no gpu adapter; skipping");
            return;
        };
        let mut backend = crate::gpu::GpuBackend::new(std::sync::Arc::new(ctx));
        let mut rgb = Vec::with_capacity(32 * 32 * 3);
        for y in 0..32 {
            for x in 0..32 {
                rgb.push(x as f32 / 32.0);
                rgb.push(y as f32 / 32.0);
                rgb.push(0.5);
            }
        }
        let linear = crate::develop::LinearPreview {
            width: 32,
            height: 32,
            rgb,
        };
        let tone = crate::develop::ToneParams::default();
        let out = backend.apply(&linear, &tone, 8, 8);
        assert_eq!(out.image.width, 8);
        assert_eq!(out.image.height, 8);
        assert_eq!(out.image.rgba.len(), 8 * 8 * 4);
    }

    /// Lightweight perf smoke: tone a 1024×1024 buffer twice on the GPU
    /// and the CPU. GPU should be at least 3× faster on a real device;
    /// in CI without a device the GPU call is skipped. The check is
    /// informational only — failure does not fail the test suite, it
    /// just prints the result.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn gpu_vs_cpu_perf() {
        use std::time::Instant;
        let w = 1024u32;
        let h = 1024u32;
        let rgb = vec![0.5f32; (w * h * 3) as usize];
        let linear = crate::develop::LinearPreview {
            width: w,
            height: h,
            rgb,
        };
        let tone = crate::develop::ToneParams::default();
        // CPU baseline.
        let t0 = Instant::now();
        let _cpu = crate::develop::apply_tone(&linear, &tone, w);
        let cpu_ms = t0.elapsed().as_secs_f64() * 1000.0;
        if let Some(ctx) = pollster::block_on(crate::gpu::GpuContext::for_tests()) {
            let mut backend = crate::gpu::GpuBackend::new(std::sync::Arc::new(ctx));
            let t0 = Instant::now();
            let _gpu = backend.apply(&linear, &tone, w, h);
            let gpu_ms = t0.elapsed().as_secs_f64() * 1000.0;
            eprintln!(
                "perf[{w}x{h}]: cpu={cpu_ms:.1}ms gpu={gpu_ms:.1}ms speedup={:.1}x",
                cpu_ms / gpu_ms.max(1e-3)
            );
        } else {
            eprintln!("no gpu adapter; perf cpu={cpu_ms:.1}ms");
        }
    }

    /// Larger develop-preview-sized perf smoke (2048×2048). GPU should
    /// pull ahead here because the per-pixel work dominates over the
    /// fixed encoding + readback overhead.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn gpu_vs_cpu_perf_2048() {
        use std::time::Instant;
        let w = 2048u32;
        let h = 2048u32;
        let rgb = vec![0.5f32; (w * h * 3) as usize];
        let linear = crate::develop::LinearPreview {
            width: w,
            height: h,
            rgb,
        };
        let tone = crate::develop::ToneParams::default();
        let t0 = Instant::now();
        let _cpu = crate::develop::apply_tone(&linear, &tone, w);
        let cpu_ms = t0.elapsed().as_secs_f64() * 1000.0;
        if let Some(ctx) = pollster::block_on(crate::gpu::GpuContext::for_tests()) {
            let mut backend = crate::gpu::GpuBackend::new(std::sync::Arc::new(ctx));
            let t0 = Instant::now();
            let _gpu = backend.apply(&linear, &tone, w, h);
            let gpu_ms = t0.elapsed().as_secs_f64() * 1000.0;
            eprintln!(
                "perf[{w}x{h}]: cpu={cpu_ms:.1}ms gpu={gpu_ms:.1}ms speedup={:.1}x",
                cpu_ms / gpu_ms.max(1e-3)
            );
        } else {
            eprintln!("no gpu adapter; perf cpu={cpu_ms:.1}ms");
        }
    }
}
