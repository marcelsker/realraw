//! Tone backend abstraction: CPU (existing pipeline) or GPU (compute
//! pipeline). The CPU backend is a faithful adapter over
//! [`crate::develop::pipeline`] so the develop / export / thumbnail code
//! doesn't need to fork.
//!
//! The backend owns per-photo state (linear texture upload, stage cache).
//! `apply` takes `&mut self` and must be called from the thread that
//! owns the wgpu device — typically the egui update thread for the
//! develop path, or a dedicated task for export / library thumbs.

use std::sync::Arc;

use crate::develop::settings::ToneParams;
use crate::develop::{LinearPreview, PreviewImage, PreviewSource};

/// Outcome of one tone application. `image` is always populated; `gpu`
/// is `Some` only for the GPU backend and lets the caller skip a CPU
/// readback by handing the wgpu texture straight to egui.
pub struct ToneOutput {
    pub image: PreviewImage,
    pub gpu: Option<GpuToneOutput>,
}

pub struct GpuToneOutput {
    pub texture: wgpu::Texture,
    pub srgb_view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
}

/// The tone dispatch. Owned (not shared) — one per photo.
pub enum ToneBackend {
    Cpu(CpuBackend),
    Gpu(Box<GpuBackend>),
}

impl ToneBackend {
    /// Construct a backend. `gpu` is the shared [`GpuContext`]; if `None`
    /// we always return a CPU backend.
    pub fn new(gpu: Option<Arc<crate::gpu::GpuContext>>) -> Self {
        match gpu {
            Some(gpu) => ToneBackend::Gpu(Box::new(GpuBackend::new(gpu))),
            None => ToneBackend::Cpu(CpuBackend),
        }
    }

    pub fn is_gpu(&self) -> bool {
        matches!(self, ToneBackend::Gpu(_))
    }

    /// Drop the cached GPU state. Call when the active linear buffer
    /// changes (e.g. switching photos).
    pub fn invalidate(&mut self) {
        if let ToneBackend::Gpu(b) = self {
            b.invalidate();
        }
    }

    /// Apply `tone` to `linear`, producing an sRGB-encoded RGBA8
    /// preview at the requested output size.
    pub fn apply(
        &mut self,
        linear: &LinearPreview,
        tone: &ToneParams,
        out_w: u32,
        out_h: u32,
    ) -> ToneOutput {
        match self {
            ToneBackend::Cpu(b) => b.apply(linear, tone, out_w, out_h),
            ToneBackend::Gpu(b) => b.apply(linear, tone, out_w, out_h),
        }
    }
}

/// CPU backend: thin shim over `apply_tone`.
pub struct CpuBackend;

impl CpuBackend {
    pub fn apply(
        &self,
        linear: &LinearPreview,
        tone: &ToneParams,
        out_w: u32,
        out_h: u32,
    ) -> ToneOutput {
        let img = crate::develop::apply_tone(linear, tone, out_w.max(out_h));
        ToneOutput {
            image: img,
            gpu: None,
        }
    }
}

/// GPU backend: dispatches the tone compute pipeline. Holds the
/// `LinearPreview` upload as a `Rgba32Float` texture plus the stage
/// cache across calls.
pub struct GpuBackend {
    gpu: Arc<crate::gpu::GpuContext>,
    linear_tex: Option<(wgpu::Texture, u32, u32)>,
    cache: crate::gpu::stage_cache::GpuStageCache,
}

impl GpuBackend {
    pub fn new(gpu: Arc<crate::gpu::GpuContext>) -> Self {
        Self {
            gpu,
            linear_tex: None,
            cache: crate::gpu::stage_cache::GpuStageCache::new(),
        }
    }

    pub fn invalidate(&mut self) {
        self.linear_tex = None;
        self.cache.clear();
    }

    fn ensure_linear_tex(&mut self, linear: &LinearPreview) -> wgpu::Texture {
        let needs_new = self
            .linear_tex
            .as_ref()
            .is_none_or(|(_, w, h)| *w != linear.width || *h != linear.height);
        if needs_new {
            let tex = crate::gpu::stage_cache::upload_linear_rgb(
                self.gpu.device(),
                self.gpu.queue(),
                &linear.rgb,
                linear.width,
                linear.height,
            );
            self.linear_tex = Some((tex, linear.width, linear.height));
            self.cache.clear();
        }
        self.linear_tex.as_ref().unwrap().0.clone()
    }

    pub fn apply(
        &mut self,
        linear: &LinearPreview,
        tone: &ToneParams,
        out_w: u32,
        out_h: u32,
    ) -> ToneOutput {
        let out_w = out_w.max(1);
        let out_h = out_h.max(1);
        let linear_tex = self.ensure_linear_tex(linear);
        let linear_view = linear_tex.create_view(&wgpu::TextureViewDescriptor::default());

        let downscale = if out_w == linear.width && out_h == linear.height {
            0
        } else {
            2
        };

        // Pre-allocate output pair; the reference we keep is `&mut
        // OutputPair` so the dispatch encoder can borrow its storage view.
        let (storage, display, storage_view, display_view) = {
            let out = crate::gpu::stage_cache::ensure_output_tex(
                self.gpu.device(),
                &mut self.cache.output,
                out_w,
                out_h,
            );
            (
                out.storage.clone(),
                out.display.clone(),
                out.storage_view.clone(),
                out.display_view.clone(),
            )
        };

        // Fused single dispatch: balance + curves + sRGB OETF + saturation
        // + orient + downscale + RGBA8 quantize, all in one pass.
        let mut encoder = self
            .gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("realraw-tone-encoder"),
            });
        self.dispatch_tone_into(
            &mut encoder,
            &linear_view,
            &storage_view,
            tone,
            out_w,
            out_h,
            linear.width,
            linear.height,
            1,
            downscale,
        );
        // Blit storage → display (both Rgba8Unorm bytes, format differs).
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &storage,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &display,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: out_w,
                height: out_h,
                depth_or_array_layers: 1,
            },
        );
        self.gpu.queue().submit([encoder.finish()]);

        // Readback for the CPU code path / fallback.
        let rgba = self.readback(&storage, out_w, out_h);

        ToneOutput {
            image: PreviewImage {
                width: out_w,
                height: out_h,
                rgba,
                source: PreviewSource::Demosaic,
            },
            gpu: Some(GpuToneOutput {
                texture: display,
                srgb_view: display_view,
                width: out_w,
                height: out_h,
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_tone_into(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        src: &wgpu::TextureView,
        dst: &wgpu::TextureView,
        tone: &ToneParams,
        out_w: u32,
        out_h: u32,
        src_w: u32,
        src_h: u32,
        orient: u32,
        downscale: u32,
    ) {
        let params = crate::gpu::pipeline::ToneParamsGpu {
            exposure: tone.exposure,
            temp: tone.temp / 100.0,
            tint: tone.tint / 100.0,
            saturation: tone.saturation / 100.0,
            contrast: tone.contrast / 100.0,
            highlights: tone.highlights / 100.0,
            shadows: tone.shadows / 100.0,
            whites: tone.whites / 100.0,
            blacks: tone.blacks / 100.0,
            out_w,
            out_h,
            src_w,
            src_h,
            apply_orient: orient,
            apply_downscale: downscale,
            _pad: 0,
        };
        let params_buf = self
            .gpu
            .device()
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("realraw-tone-params"),
                contents: bytemuck::bytes_of(&params),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            });
        let bind_group = self.gpu.tone_pipeline().make_bind_group(
            self.gpu.device(),
            &params_buf,
            src,
            dst,
        );
        let mut cpass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("realraw-tone-cpass"),
            timestamp_writes: None,
        });
        cpass.set_pipeline(self.gpu.tone_pipeline().pipeline());
        cpass.set_bind_group(0, &bind_group, &[]);
        let gx = out_w.div_ceil(8);
        let gy = out_h.div_ceil(8);
        cpass.dispatch_workgroups(gx, gy, 1);
    }

    fn readback(&self, tex: &wgpu::Texture, width: u32, height: u32) -> Vec<u8> {
        let unpadded_bpr = width * 4;
        let padded_bpr = (unpadded_bpr + 255) & !255;
        let buf_size = (padded_bpr * height) as u64;
        let read_buf = self.gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("realraw-tone-readback"),
            size: buf_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("realraw-tone-readback-encoder"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &read_buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bpr),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.gpu.queue().submit([encoder.finish()]);

        let slice = read_buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.gpu.device().poll(wgpu::Maintain::Wait);
        rx.recv().expect("map_async callback").expect("map_async ok");

        let mapped = slice.get_mapped_range();
        let mut rgba = vec![0u8; (unpadded_bpr * height) as usize];
        for y in 0..height as usize {
            let src = &mapped[y * padded_bpr as usize..y * padded_bpr as usize + unpadded_bpr as usize];
            let dst = &mut rgba[y * unpadded_bpr as usize..(y + 1) * unpadded_bpr as usize];
            dst.copy_from_slice(src);
        }
        drop(mapped);
        read_buf.unmap();
        rgba
    }
}

use wgpu::util::DeviceExt as _;
