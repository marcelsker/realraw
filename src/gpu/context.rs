//! Thin wrapper around the eframe-provided wgpu device/queue.
//!
//! The context is constructed once, on the first frame where
//! [`eframe::Frame::wgpu_render_state`] is available, and then shared via
//! [`std::sync::Arc`] so background tasks (export, library thumbnail
//! regen) can submit work to the same device.

use std::sync::Arc;

use eframe::egui_wgpu;

/// Shared handle to the application's wgpu device + queue.
///
/// Cloning is cheap: it is a single [`Arc`] clone. The inner types are
/// `Send + Sync` on all desktop targets (naga 24 / wgpu 24 mark them as
/// `WasmNotSendSync`, which means `Send + Sync` outside wasm).
#[derive(Clone)]
pub struct GpuContext {
    inner: Arc<Inner>,
}

struct Inner {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Always `Rgba8UnormSrgb` in practice — required by
    /// [`egui_wgpu::Renderer::register_native_texture`].
    target_format: wgpu::TextureFormat,
    /// Compute pipelines are owned by the device; keep them on the
    /// `Inner` so the same context exposes a single canonical pipeline
    /// layout for both develop and export.
    tone: crate::gpu::pipeline::TonePipeline,
    /// Adapter info for diagnostics / feature detection.
    adapter_info: wgpu::AdapterInfo,
}

impl std::fmt::Debug for GpuContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuContext")
            .field("adapter", &self.inner.adapter_info.name)
            .field("target_format", &self.inner.target_format)
            .finish_non_exhaustive()
    }
}

impl GpuContext {
    /// Try to create a context from the wgpu render state that eframe
    /// hands us on the first frame. Returns `None` if wgpu is not
    /// available (e.g. user is on the `glow` backend).
    pub fn from_render_state(rs: &egui_wgpu::RenderState) -> Option<Self> {
        let device = rs.device.clone();
        let queue = rs.queue.clone();
        let target_format = rs.target_format;
        let adapter_info = rs.adapter.get_info();
        let tone = crate::gpu::pipeline::TonePipeline::new(&device, target_format);
        Some(Self {
            inner: Arc::new(Inner {
                device,
                queue,
                target_format,
                tone,
                adapter_info,
            }),
        })
    }

    /// Best-effort fallback for tests and code paths that want to spin
    /// up a context without an eframe window. Picks the default adapter
    /// and a sensible surface-less device. Blocks on adapter request.
    #[cfg(test)]
    pub async fn for_tests() -> Option<Self> {
        let _ = wgpu::util::initialize_adapter_from_env_or_default; // touch the import
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });
        let adapter = wgpu::util::initialize_adapter_from_env_or_default(&instance, None).await?;
        let target_format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("realraw-test-gpu"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                    memory_hints: wgpu::MemoryHints::default(),
                },
                None,
            )
            .await
            .ok()?;
        let tone = crate::gpu::pipeline::TonePipeline::new(&device, target_format);
        let adapter_info = adapter.get_info();
        Some(Self {
            inner: Arc::new(Inner {
                device,
                queue,
                target_format,
                tone,
                adapter_info,
            }),
        })
    }

    #[inline]
    pub fn device(&self) -> &wgpu::Device {
        &self.inner.device
    }

    #[inline]
    pub fn queue(&self) -> &wgpu::Queue {
        &self.inner.queue
    }

    #[inline]
    pub fn target_format(&self) -> wgpu::TextureFormat {
        self.inner.target_format
    }

    #[inline]
    pub fn tone_pipeline(&self) -> &crate::gpu::pipeline::TonePipeline {
        &self.inner.tone
    }

    /// One-line adapter name for diagnostics (e.g. status bar).
    pub fn adapter_label(&self) -> String {
        let a = &self.inner.adapter_info;
        format!("{} ({:?})", a.name, a.backend)
    }
}
