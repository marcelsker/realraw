//! Compute pipeline wrapper: bind group layout, pipeline layout, pipeline,
//! and the host-side [`ToneParamsGpu`] uniform that drives the kernel.

use bytemuck::{Pod, Zeroable};

/// WGSL `ToneParams` mirror. Layout must match `shaders/tone.wgsl`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct ToneParamsGpu {
    pub exposure: f32,
    pub temp: f32,
    pub tint: f32,
    pub saturation: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub out_w: u32,
    pub out_h: u32,
    pub src_w: u32,
    pub src_h: u32,
    pub apply_orient: u32,
    pub apply_downscale: u32,
    pub _pad: u32,
}

impl Default for ToneParamsGpu {
    fn default() -> Self {
        Self {
            exposure: 0.0,
            temp: 0.0,
            tint: 0.0,
            saturation: 0.0,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            out_w: 0,
            out_h: 0,
            src_w: 0,
            src_h: 0,
            apply_orient: 1,
            apply_downscale: 0,
            _pad: 0,
        }
    }
}

/// How to apply EXIF orientation in the fused pass.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Orient {
    /// No rotation / flip; pass 1.
    Identity,
    /// Raw EXIF value 1..=8.
    Exif(u8),
}

impl Orient {
    pub fn as_u32(self) -> u32 {
        match self {
            Orient::Identity => 1,
            Orient::Exif(v) => v as u32,
        }
    }
}

/// Owns the wgpu compute pipeline, bind group layout, and the bind group
/// factory. The bind group changes per dispatch (different input/output
/// textures), so we expose [`Self::make_bind_group`] and don't cache it.
pub struct TonePipeline {
    layout: wgpu::PipelineLayout,
    pipeline: wgpu::ComputePipeline,
    bind_layout: wgpu::BindGroupLayout,
}

impl TonePipeline {
    /// Compile the WGSL and build the pipeline. `target_format` is unused
    /// for compute but is part of the constructor signature for symmetry
    /// with future render pipelines.
    pub fn new(device: &wgpu::Device, _target_format: wgpu::TextureFormat) -> Self {
        // Validate before handing to wgpu. This produces the nicest error
        // message in the common case (typo'd WGSL).
        if let Err(e) = crate::gpu::compile::validate() {
            panic!("{e}");
        }
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("realraw-tone"),
            source: wgpu::ShaderSource::Wgsl(
                crate::gpu::compile::tone_source().as_ref().into(),
            ),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("realraw-tone-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(
                            std::mem::size_of::<ToneParamsGpu>() as u64,
                        ),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("realraw-tone-pl"),
            bind_group_layouts: &[&bind_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("realraw-tone-cs"),
            layout: Some(&layout),
            module: &module,
            entry_point: Some("cs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });

        Self {
            layout,
            pipeline,
            bind_layout,
        }
    }

    pub fn layout(&self) -> &wgpu::PipelineLayout {
        &self.layout
    }

    pub fn pipeline(&self) -> &wgpu::ComputePipeline {
        &self.pipeline
    }

    pub fn bind_layout(&self) -> &wgpu::BindGroupLayout {
        &self.bind_layout
    }

    /// Build the bind group for one dispatch. The caller owns `params_buf`,
    /// `src_view`, and `out_view` for the duration of the dispatch.
    pub fn make_bind_group(
        &self,
        device: &wgpu::Device,
        params_buf: &wgpu::Buffer,
        src_view: &wgpu::TextureView,
        out_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("realraw-tone-bg"),
            layout: &self.bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(src_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(out_view),
                },
            ],
        })
    }
}
