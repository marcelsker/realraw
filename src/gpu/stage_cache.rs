//! GPU-side stage cache for the develop tone pipeline.
//!
//! Replaces the CPU `Vec<f32>` `StageCache` when the GPU backend is in
//! use. Layout:
//!
//! * `balanced: Rgba32Float` — output of the WB × exposure pass.
//! * `curved: Rgba32Float`   — output of the luminance-curve pass.
//! * `output: Rgba8UnormSrgb` — final view-size RGBA (created lazily per
//!   view size; the only texture that grows on resize).
//!
//! The cache invalidates the same way the CPU version does: by
//! `(params_hash, width, height)` triple + a TTL.

use std::time::Instant;

use crate::develop::settings::ToneParams;

const CACHE_TTL: std::time::Duration = std::time::Duration::from_millis(200);

#[derive(Copy, Clone)]
pub struct CacheKey {
    hash: u64,
    width: u32,
    height: u32,
    cached_at: Instant,
}

impl CacheKey {
    fn valid(&self, hash: u64, w: u32, h: u32) -> bool {
        self.hash == hash
            && self.width == w
            && self.height == h
            && self.cached_at.elapsed() < CACHE_TTL
    }
}

pub struct GpuStageCache {
    pub balanced: Option<(wgpu::Texture, CacheKey)>,
    pub curved: Option<(wgpu::Texture, CacheKey)>,
    /// View-size output pair (storage + display). Reallocated when the
    /// requested size changes.
    pub output: Option<OutputPair>,
}

impl Default for GpuStageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuStageCache {
    pub fn new() -> Self {
        Self {
            balanced: None,
            curved: None,
            output: None,
        }
    }

    pub fn clear(&mut self) {
        self.balanced = None;
        self.curved = None;
        self.output = None;
    }

    /// Hash for Stage 1 (balance = exposure × WB).
    pub fn hash_balance(tone: &ToneParams) -> u64 {
        tone.exposure.to_bits() as u64
            ^ (tone.temp.to_bits() as u64).rotate_left(13)
            ^ (tone.tint.to_bits() as u64).rotate_left(26)
    }

    /// Hash for Stage 2 (curves).
    pub fn hash_curves(tone: &ToneParams) -> u64 {
        (tone.contrast.to_bits() as u64)
            ^ (tone.highlights.to_bits() as u64).rotate_left(8)
            ^ (tone.shadows.to_bits() as u64).rotate_left(16)
            ^ (tone.whites.to_bits() as u64).rotate_left(24)
            ^ (tone.blacks.to_bits() as u64).rotate_left(32)
    }

    pub fn balanced_valid(&self, tone: &ToneParams, w: u32, h: u32) -> bool {
        self.balanced
            .as_ref()
            .is_some_and(|(_, k)| k.valid(Self::hash_balance(tone), w, h))
    }

    pub fn curved_valid(&self, tone: &ToneParams, w: u32, h: u32) -> bool {
        self.curved
            .as_ref()
            .is_some_and(|(_, k)| k.valid(Self::hash_curves(tone), w, h))
    }

    pub fn record_balanced(&mut self, tone: &ToneParams, tex: wgpu::Texture, w: u32, h: u32) {
        self.balanced = Some((
            tex,
            CacheKey {
                hash: Self::hash_balance(tone),
                width: w,
                height: h,
                cached_at: Instant::now(),
            },
        ));
    }

    pub fn record_curved(&mut self, tone: &ToneParams, tex: wgpu::Texture, w: u32, h: u32) {
        self.curved = Some((
            tex,
            CacheKey {
                hash: Self::hash_curves(tone),
                width: w,
                height: h,
                cached_at: Instant::now(),
            },
        ));
    }
}

/// Linear f32 RGB → Rgba32Float texture upload. Pads the alpha channel
/// with 1.0 (input data is interleaved RGB only).
pub fn upload_linear_rgb(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    rgb: &[f32],
    width: u32,
    height: u32,
) -> wgpu::Texture {
    assert_eq!(
        rgb.len(),
        (width as usize) * (height as usize) * 3,
        "rgb buffer size must match 3*w*h"
    );
    // Convert RGB float -> RGBA float with alpha = 1.0 in a single
    // staging buffer.
    let pixels = (width as usize) * (height as usize);
    let mut rgba = Vec::with_capacity(pixels * 4);
    for chunk in rgb.chunks_exact(3) {
        rgba.push(chunk[0]);
        rgba.push(chunk[1]);
        rgba.push(chunk[2]);
        rgba.push(1.0);
    }
    let bytes: &[u8] = bytemuck::cast_slice(&rgba);
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("realraw-linear-tex"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 16),
            rows_per_image: None,
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    tex
}

/// One pair of output textures: a `Rgba8Unorm` storage texture the
/// compute shader writes to, and a `Rgba8UnormSrgb` display texture
/// that egui samples. We can't have a single texture because the
/// storage-binding view format must equal the texture format
/// (`Rgba8UnormSrgb` is not storage-bindable in wgpu 24), so we keep
/// two separate textures and blit storage→display every dispatch.
pub struct OutputPair {
    pub storage: wgpu::Texture,
    pub display: wgpu::Texture,
    pub storage_view: wgpu::TextureView,
    pub display_view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
}

/// Allocate (or reuse) the view-size output pair.
pub fn ensure_output_tex<'a>(
    device: &'a wgpu::Device,
    cache: &'a mut Option<OutputPair>,
    width: u32,
    height: u32,
) -> &'a mut OutputPair {
    let needs_new = cache
        .as_ref()
        .is_none_or(|p| p.width != width || p.height != height);
    if needs_new {
        let storage = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("realraw-output-tex-storage"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let display = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("realraw-output-tex-display"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let storage_view = storage.create_view(&wgpu::TextureViewDescriptor {
            label: Some("realraw-output-tex-storage-view"),
            ..Default::default()
        });
        let display_view = display.create_view(&wgpu::TextureViewDescriptor {
            label: Some("realraw-output-tex-display-view"),
            ..Default::default()
        });
        *cache = Some(OutputPair {
            storage,
            display,
            storage_view,
            display_view,
            width,
            height,
        });
    }
    cache.as_mut().expect("just set")
}

/// Allocate an intermediate Rgba32Float texture (balanced / curved stages).
pub fn create_intermediate_tex(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    label: &'static str,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING,
        view_formats: &[],
    })
}
