//! GPU-accelerated image processing for develop tone, export, and library thumbs.
//!
//! ## Backend
//!
//! The develop tone stage, export, and library thumbnail regeneration all
//! share the same compute pipeline: a fused pass that takes a linear f32 RGB
//! texture and produces a tone-mapped sRGB Rgba8Unorm texture.
//!
//! WGSL source lives in `shaders/`. naga validates the source at startup;
//! `wgpu::Device::create_shader_module` then takes the WGSL directly.
//!
//! ## CPU fallback
//!
//! If wgpu init fails (broken driver, headless CI, etc.) every entry point
//! here returns `None` and the existing CPU pipeline in
//! [`crate::develop::pipeline`] stays the only path. See
//! [`crate::gpu::backend::ToneBackend`] for the dispatch.

pub mod backend;
pub mod compile;
pub mod context;
pub mod pipeline;
pub mod stage_cache;

pub use backend::{CpuBackend, GpuBackend, ToneBackend};
pub use context::GpuContext;
pub use pipeline::ToneParamsGpu;
