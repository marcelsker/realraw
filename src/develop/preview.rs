//! Background progressive preview loader for Develop mode.
//!
//! Stages:
//! 1. Placeholder (disk sRGB JPEG cache / library thumb / embedded JPEG)
//! 2. Linear demosaic → [`LinearPreview`]: load from disk linear cache if
//!    present, otherwise rawler demosaic + write cache
//! 3. Tone (light panel → sRGB) — re-run on slider changes with
//!    generation-based cancel / coalescing (uses the in-RAM linear buffer)

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;

use super::decode::{
    decode_embedded_preview, decode_raw_preview, PreviewImage, PreviewSource, PREVIEW_MAX_DIM,
};
use super::pipeline::{
    apply_balance, apply_curves, apply_output, apply_tone, develop_linear_with_progress,
    downscale_rgb_nearest, proxy_dims, LinearPreview,
};
use super::settings::ToneParams;
use crate::catalog::{preview_cache, thumbnail_cache};
use crate::gpu::{GpuBackend, GpuContext};

/// Crossfade duration from thumbnail → demosaic (seconds).
const FADE_SECS: f32 = 0.0;

/// Divisor applied to the on-screen render size while a slider is being
/// dragged. The egui texture is uploaded at this lower resolution and
/// bilinear-upscaled to the display rect, which is roughly 4× faster on
/// CPU and noticeably snappier on GPU. On drag-stop the full-resolution
/// pass runs and snaps in within one frame.
const DRAG_QUALITY_DIVISOR: u32 = 2;
/// Hard floor for the dragged render dim; tiny panels stay readable.
const DRAG_QUALITY_MIN_DIM: u32 = 128;

/// TTL for intermediate stage caches (balance, curves).
/// During active slider dragging (~100ms between events) the cache stays hot.
/// After the user stops, it expires quickly so stale entries don't persist.
const CACHE_TTL: Duration = Duration::from_millis(200);

/// Hash the params that feed into Stage 1 (balance: exposure × WB gains).
fn hash_balance(exposure: f32, temp: f32, tint: f32) -> u64 {
    (exposure.to_bits() as u64)
        ^ (temp.to_bits() as u64).rotate_left(13)
        ^ (tint.to_bits() as u64).rotate_left(26)
}

/// Hash the params that feed into Stage 2 (curves: luminance curves).
fn hash_curves(
    contrast: f32,
    highlights: f32,
    shadows: f32,
    whites: f32,
    blacks: f32,
) -> u64 {
    (contrast.to_bits() as u64)
        ^ (highlights.to_bits() as u64).rotate_left(8)
        ^ (shadows.to_bits() as u64).rotate_left(16)
        ^ (whites.to_bits() as u64).rotate_left(24)
        ^ (blacks.to_bits() as u64).rotate_left(32)
}

/// Cache entry for an intermediate pipeline stage buffer.
struct StageCache {
    rgb: Vec<f32>,
    width: u32,
    height: u32,
    params_hash: u64,
    cached_at: Instant,
}

impl StageCache {
    fn valid(&self, params_hash: u64, width: u32, height: u32) -> bool {
        self.params_hash == params_hash
            && self.width == width
            && self.height == height
            && self.cached_at.elapsed() < CACHE_TTL
    }
}

struct PreviewResult {
    photo_id: i64,
    /// Photo open generation (invalidates everything for a photo switch).
    generation: u64,
    kind: ResultKind,
}

enum ResultKind {
    Placeholder(PreviewImage),
    /// Actual demosaic is running (not a linear-cache hit). `0.0..=1.0`.
    DemosaicProgress(f32),
    /// Linear buffer ready + first toned display image.
    LinearReady {
        linear: Arc<LinearPreview>,
        display: PreviewImage,
        /// Tone params used to build `display`.
        tone_used: ToneParams,
    },
    /// Linear develop failed; optional gamma-encoded fallback image.
    LinearFailed {
        error: String,
        fallback: Option<PreviewImage>,
    },
    /// Tone re-render at current on-screen preview size.
    Tone {
        tone_gen: u64,
        img: PreviewImage,
        /// Freshly-computed Stage 1 data (None if cache was reused).
        balanced_rgb: Option<Vec<f32>>,
        balanced_w: u32,
        balanced_h: u32,
        /// Freshly-computed Stage 2 data (None if cache was reused).
        curved_rgb: Option<Vec<f32>>,
    },
}

/// Owns the develop preview texture and drives progressive RAW develop.
pub struct DevelopPreview {
    pub photo_id: Option<i64>,
    generation: u64,
    /// Metadata for the committed base image (thumb or demosaic).
    image: Option<PreviewImage>,
    /// Committed base texture. Stays as the thumbnail for the whole
    /// crossfade so the base layer never pops.
    texture: Option<egui::TextureHandle>,
    /// Demosaic texture fading **in** on top of [`Self::texture`].
    /// When the fade finishes this becomes the new base.
    reveal: Option<egui::TextureHandle>,
    /// Metadata for the reveal image (kept until fade commits).
    reveal_image: Option<PreviewImage>,
    /// When the demosaic reveal fade started.
    fade_start: Option<Instant>,
    /// Locked aspect (w/h) for stable layout during load + fade.
    display_aspect: Option<f32>,
    pub status: Option<String>,
    loading: bool,
    settled: bool,
    /// Coarse demosaic progress while rawler is running. `None` for
    /// linear-cache hits, idle, or after settle.
    demosaic_progress: Option<f32>,
    /// Linear demosaic buffer for the current photo (pre-tone).
    pub linear: Option<Arc<LinearPreview>>,
    /// Current light-panel tone params.
    tone: ToneParams,
    /// True while a tone slider is being dragged.
    dragging: bool,
    /// Longest edge (physical pixels) of the on-screen preview; 0 = unknown.
    view_max_dim: u32,
    /// Bumped on every scheduled tone render.
    tone_gen: u64,
    /// A tone worker is currently running.
    tone_inflight: bool,
    /// Need another tone pass after the in-flight one finishes.
    tone_dirty: bool,
    /// Stage 1 cache: linear RGB after exposure × WB gains.
    balanced_cache: Option<StageCache>,
    /// Stage 2 cache: linear RGB after luminance curves.
    curved_cache: Option<StageCache>,
    /// Shared GPU context. `Some` enables the GPU tone worker;
    /// `None` falls back to the existing CPU path.
    gpu: Option<Arc<GpuContext>>,
    /// Per-photo GPU backend (only valid when `gpu.is_some()`).
    /// Wrapped in `Arc<Mutex<_>>` so the worker thread can dispatch.
    gpu_backend: Option<Arc<Mutex<GpuBackend>>>,
    tx: Sender<PreviewResult>,
    rx: Receiver<PreviewResult>,
}

impl Default for DevelopPreview {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            photo_id: None,
            generation: 0,
            image: None,
            texture: None,
            reveal: None,
            reveal_image: None,
            fade_start: None,
            display_aspect: None,
            status: None,
            loading: false,
            settled: false,
            demosaic_progress: None,
            linear: None,
            tone: ToneParams::default(),
            dragging: false,
            view_max_dim: 0,
            tone_gen: 0,
            tone_inflight: false,
            tone_dirty: false,
            balanced_cache: None,
            curved_cache: None,
            gpu: None,
            gpu_backend: None,
            tx,
            rx,
        }
    }
}

impl DevelopPreview {
    /// Install (or clear) the GPU context. When `Some`, tone workers
    /// dispatch on the GPU. Switching contexts or photo ids invalidates
    /// the cached GPU state automatically.
    pub fn set_gpu(&mut self, gpu: Option<Arc<GpuContext>>) {
        self.gpu = gpu.clone();
        self.gpu_backend = gpu.map(|g| Arc::new(Mutex::new(GpuBackend::new(g))));
    }

    pub fn is_active_for(&self, photo_id: i64) -> bool {
        self.photo_id == Some(photo_id) && (self.loading || self.settled || self.image.is_some())
    }

    /// Start loading `photo_id` for interactive develop.
    ///
    /// Progressive:
    /// 1. Placeholder: library thumb (reflects develop edits) → develop
    ///    sRGB cache → embedded JPEG
    /// 2. Linear demosaic → RAM buffer (disk linear cache preferred)
    /// 3. Apply tone → display texture
    pub fn open(
        &mut self,
        photo_id: i64,
        path: PathBuf,
        orientation: Option<i64>,
        catalog_dir: PathBuf,
        tone: ToneParams,
    ) {
        if self.is_active_for(photo_id) {
            return;
        }

        self.photo_id = Some(photo_id);
        self.generation = self.generation.wrapping_add(1);
        self.image = None;
        self.texture = None;
        self.reveal = None;
        self.reveal_image = None;
        self.fade_start = None;
        self.display_aspect = None;
        self.linear = None;
        self.tone = tone;
        self.dragging = false;
        // Keep view_max_dim across photo switches (panel size is stable).
        self.tone_gen = 0;
        self.tone_inflight = false;
        self.tone_dirty = false;
        self.balanced_cache = None;
        self.curved_cache = None;
        if let Some(b) = &self.gpu_backend
            && let Ok(mut g) = b.lock() {
                g.invalidate();
            }
        self.loading = true;
        self.settled = false;
        self.demosaic_progress = None;

        let job_gen = self.generation;
        let tx = self.tx.clone();
        // Tone the first demosaic paint at the current view size so the
        // base layer matches the final resolution (no mid-fade re-tone).
        let first_tone_dim = if self.view_max_dim > 0 {
            self.view_max_dim
        } else {
            1024
        };

        if !super::decode::is_raw_path(&path) {
            self.loading = false;
            self.settled = true;
            self.status = Some("Develop preview supports RAW files only".into());
            return;
        }

        self.status = Some("Loading…".into());

        thread::Builder::new()
            .name("develop-preview".into())
            .spawn(move || {
                // Phase 1: fast paint. Prefer library thumb — it is rewritten
                // on develop edits — over the identity develop preview cache,
                // which is otherwise stale after tone changes.
                let placeholder = load_cached_thumb(&catalog_dir, photo_id)
                    .or_else(|| preview_cache::load_preview(&catalog_dir, photo_id))
                    .or_else(|| decode_embedded_preview(&path, orientation).ok());
                if let Some(img) = placeholder {
                    let _ = tx.send(PreviewResult {
                        photo_id,
                        generation: job_gen,
                        kind: ResultKind::Placeholder(img),
                    });
                }

                // Phase 2: linear demosaic — prefer on-disk linear cache so we
                // do not re-run rawler when reopening the same photo.
                // Progress is only reported on a real demosaic (cache miss).
                let mut demosaic_report: Option<Box<dyn FnMut(f32)>> = None;
                let linear = match preview_cache::load_linear(&catalog_dir, photo_id, orientation)
                {
                    Some(lin) => Ok(lin),
                    None => {
                        let tx_p = tx.clone();
                        let mut report = move |p: f32| {
                            let _ = tx_p.send(PreviewResult {
                                photo_id,
                                generation: job_gen,
                                kind: ResultKind::DemosaicProgress(p.clamp(0.0, 1.0)),
                            });
                        };
                        let result =
                            develop_linear_with_progress(
                                &path,
                                orientation,
                                PREVIEW_MAX_DIM,
                                &mut report,
                            );
                        demosaic_report = Some(Box::new(report));
                        result
                    }
                };

                match linear {
                    Ok(linear) => {
                        // Tone the on-screen image first and deliver it so the
                        // progress bar is not stuck on disk cache writes.
                        if let Some(report) = demosaic_report.as_mut() {
                            report(0.90);
                        }
                        let dim = first_tone_dim
                            .min(linear.width.max(linear.height))
                            .max(1);
                        let display = apply_tone(&linear, &tone, dim);
                        if let Some(report) = demosaic_report.as_mut() {
                            report(1.0);
                        }
                        let linear = Arc::new(linear);
                        let _ = tx.send(PreviewResult {
                            photo_id,
                            generation: job_gen,
                            kind: ResultKind::LinearReady {
                                linear: Arc::clone(&linear),
                                display,
                                tone_used: tone,
                            },
                        });

                        // Disk caches after the UI is unblocked.
                        if demosaic_report.is_some()
                            && let Err(e) = preview_cache::save_linear(
                                &catalog_dir,
                                photo_id,
                                orientation,
                                &linear,
                            ) {
                                eprintln!(
                                    "linear cache save failed for photo {photo_id}: {e}"
                                );
                            }
                        // Refresh tone-current JPEG placeholder for next open.
                        let cache_img = apply_tone(&linear, &tone, PREVIEW_MAX_DIM);
                        if let Err(e) =
                            preview_cache::save_preview(&catalog_dir, photo_id, &cache_img)
                        {
                            eprintln!("preview cache save failed for photo {photo_id}: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("linear develop failed for {}: {e}", path.display());
                        let fallback = decode_raw_preview(&path, orientation).ok();
                        if let Some(ref img) = fallback {
                            let _ = preview_cache::save_preview(&catalog_dir, photo_id, img);
                        }
                        let _ = tx.send(PreviewResult {
                            photo_id,
                            generation: job_gen,
                            kind: ResultKind::LinearFailed {
                                error: e.to_string(),
                                fallback,
                            },
                        });
                    }
                }
            })
            .expect("spawn develop-preview");
    }

    /// Update light-panel tone. Re-tones at the current on-screen preview size.
    /// In-flight tone jobs are superseded via `tone_gen` (render cancelling).
    pub fn set_tone(&mut self, tone: ToneParams, dragging: bool) {
        let changed = !self.tone.approx_eq(&tone) || self.dragging != dragging;
        self.tone = tone;
        self.dragging = dragging;
        if !changed && !self.tone_dirty {
            return;
        }
        if self.linear.is_none() {
            return;
        }
        self.schedule_tone();
    }

    /// Sample linear RGB from the demosaiced buffer at normalized UV coords.
    ///
    /// Averages a 5×5 neighbourhood for noise reduction. Returns `None` if the
    /// linear buffer is not yet available or the coordinates are out of range.
    pub fn sample_pixel(&self, u: f32, v: f32) -> Option<(f32, f32, f32)> {
        let linear = self.linear.as_ref()?;
        let w = linear.width as f32;
        let h = linear.height as f32;
        let cx = (u * w).round().clamp(0.0, w - 1.0) as i32;
        let cy = (v * h).round().clamp(0.0, h - 1.0) as i32;
        let radius = 2;
        let mut sum_r = 0.0f64;
        let mut sum_g = 0.0f64;
        let mut sum_b = 0.0f64;
        let mut count = 0u64;
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let px = cx + dx;
                let py = cy + dy;
                if px < 0 || px >= linear.width as i32 || py < 0 || py >= linear.height as i32 {
                    continue;
                }
                let base = (py as usize * linear.width as usize + px as usize) * 3;
                sum_r += linear.rgb[base] as f64;
                sum_g += linear.rgb[base + 1] as f64;
                sum_b += linear.rgb[base + 2] as f64;
                count += 1;
            }
        }
        if count == 0 {
            return None;
        }
        Some((
            (sum_r / count as f64) as f32,
            (sum_g / count as f64) as f32,
            (sum_b / count as f64) as f32,
        ))
    }

    /// Report the develop central-panel viewport so tone renders match
    /// on-screen pixel size (`avail` in points × `pixels_per_point`).
    pub fn set_view_size(&mut self, avail: egui::Vec2, pixels_per_point: f32) {
        let (img_w, img_h) = if let Some(lin) = &self.linear {
            (lin.width, lin.height)
        } else if let Some(img) = &self.image {
            (img.width, img.height)
        } else {
            // No aspect yet — use the panel's long edge.
            let dim = (avail.x.max(avail.y) * pixels_per_point)
                .ceil()
                .clamp(1.0, PREVIEW_MAX_DIM as f32) as u32;
            self.update_view_max_dim(dim);
            return;
        };

        let dim = fit_view_max_dim(avail, pixels_per_point, img_w, img_h);
        self.update_view_max_dim(dim);
    }

    fn update_view_max_dim(&mut self, dim: u32) {
        let dim = dim.clamp(1, PREVIEW_MAX_DIM);
        if dim == self.view_max_dim {
            return;
        }
        let prev = self.view_max_dim;
        self.view_max_dim = dim;
        // Never re-tone mid-fade (size swap flashes). Defer via tone_dirty.
        if self.fading() {
            if self.linear.is_some() && (prev == 0 || dim_changed_enough(prev, dim)) {
                self.tone_dirty = true;
            }
            return;
        }
        // Re-tone when size changes meaningfully (resize / first known size).
        if self.linear.is_some() && (prev == 0 || dim_changed_enough(prev, dim)) {
            self.schedule_tone();
        }
    }

    fn tone_max_dim(&self) -> u32 {
        let dim = if self.view_max_dim > 0 {
            self.view_max_dim
        } else {
            // Before the first layout pass, avoid a full 2048 tone.
            1024
        };
        if let Some(lin) = &self.linear {
            dim.min(lin.width.max(lin.height))
        } else {
            dim
        }
    }

    /// Effective render dim: full `tone_max_dim()` while idle, divided
    /// by [`DRAG_QUALITY_DIVISOR`] (clamped) while a slider is being
    /// dragged. The lower-res output is bilinear-upscaled to the display
    /// rect by egui; on drag-stop the next tone pass runs at the full
    /// size and snaps in.
    fn render_max_dim(&self) -> u32 {
        if !self.dragging {
            return self.tone_max_dim();
        }
        let full = self.tone_max_dim();
        (full / DRAG_QUALITY_DIVISOR).max(DRAG_QUALITY_MIN_DIM)
    }

    fn fading(&self) -> bool {
        self.reveal.is_some()
    }

    fn schedule_tone(&mut self) {
        self.tone_gen = self.tone_gen.wrapping_add(1);
        self.tone_dirty = true;
        self.try_spawn_tone();
    }

    fn try_spawn_tone(&mut self) {
        if !self.tone_dirty {
            return;
        }
        // Hold re-tones until the thumb overlay has fully dissolved.
        if self.fading() {
            return;
        }
        let Some(linear) = self.linear.clone() else {
            return;
        };
        let Some(photo_id) = self.photo_id else {
            return;
        };

        self.tone_dirty = false;
        self.tone_inflight = true;
        let tone_gen = self.tone_gen;
        let generation = self.generation;
        let tone = self.tone;
        // Drag-quality: render at a fraction of the on-screen size while
        // a slider is held. The output texture is bilinear-upscaled to
        // the display rect by egui, so the user sees a slightly fuzzy
        // preview that updates much faster. Drag-stop triggers a normal
        // `set_tone(_, false)` call which routes through here with the
        // full dim.
        let max_dim = self.render_max_dim();
        let tx = self.tx.clone();

        // GPU path: the heavy pixel work runs on the device; the
        // worker thread mostly blocks on `Device::poll` / `map_async`.
        if let Some(gpu_backend) = self.gpu_backend.clone() {
            thread::Builder::new()
                .name("develop-tone-gpu".into())
                .spawn(move || {
                    let out = match gpu_backend.lock() {
                        Ok(mut backend) => {
                            let (pw, ph) = proxy_dims(linear.width, linear.height, max_dim);
                            let (out_w, out_h) = if linear.width <= max_dim
                                && linear.height <= max_dim
                            {
                                (linear.width, linear.height)
                            } else {
                                (pw, ph)
                            };
                            let result = backend.apply(&linear, &tone, out_w, out_h);
                            result.image
                        }
                        Err(_) => return,
                    };
                    let _ = tx.send(PreviewResult {
                        photo_id,
                        generation,
                        kind: ResultKind::Tone {
                            tone_gen,
                            img: out,
                            balanced_rgb: None,
                            balanced_w: 0,
                            balanced_h: 0,
                            curved_rgb: None,
                        },
                    });
                })
                .expect("spawn develop-tone-gpu");
            return;
        }

        // CPU path: stage-cache aware, identical to before.
        // Determine which stage caches are still valid.
        let (proxy_w, proxy_h) = proxy_dims(linear.width, linear.height, max_dim);
        let bhash = hash_balance(tone.exposure, tone.temp, tone.tint);
        let chash = hash_curves(
            tone.contrast,
            tone.highlights,
            tone.shadows,
            tone.whites,
            tone.blacks,
        );

        let prev_balanced = self
            .balanced_cache
            .as_ref()
            .filter(|c| c.valid(bhash, proxy_w, proxy_h))
            .map(|c| Arc::from(c.rgb.as_slice()));

        // Curves cache is only valid if balance cache is also valid (it
        // depends on the balanced data, not just the curve params).
        let prev_curved = match prev_balanced {
            Some(_) => self
                .curved_cache
                .as_ref()
                .filter(|c| c.valid(chash, proxy_w, proxy_h))
                .map(|c| Arc::from(c.rgb.as_slice())),
            None => None,
        };

        thread::Builder::new()
            .name("develop-tone".into())
            .spawn(move || {
                let (pw, ph, proxy) = if linear.width <= max_dim && linear.height <= max_dim {
                    (linear.width, linear.height, linear.rgb.clone())
                } else {
                    downscale_rgb_nearest(&linear.rgb, linear.width, linear.height, max_dim)
                };

                // Stage 1: Balance (exposure × WB gains).
                let (balanced, new_balanced) = match prev_balanced {
                    Some(arc) => (arc, None),
                    None => {
                        let bal =
                            apply_balance(&proxy, pw, ph, tone.exposure, tone.temp, tone.tint);
                        (Arc::from(bal.as_slice()), Some(bal))
                    }
                };

                // Stage 2: Luminance curves (contrast, H/S/W/B).
                let (curved, new_curved) = match prev_curved {
                    Some(arc) => (arc, None),
                    None => {
                        let curv = apply_curves(
                            &balanced,
                            pw,
                            ph,
                            tone.contrast,
                            tone.highlights,
                            tone.shadows,
                            tone.whites,
                            tone.blacks,
                        );
                        (Arc::from(curv.as_slice()), Some(curv))
                    }
                };

                // Stage 3: sRGB gamma + saturation → u8 RGBA.
                let img = apply_output(&curved, pw, ph, tone.saturation);

                let _ = tx.send(PreviewResult {
                    photo_id,
                    generation,
                    kind: ResultKind::Tone {
                        tone_gen,
                        img,
                        balanced_rgb: new_balanced,
                        balanced_w: pw,
                        balanced_h: ph,
                        curved_rgb: new_curved,
                    },
                });
            })
            .expect("spawn develop-tone");
    }

    pub fn fail(&mut self, photo_id: i64, message: String) {
        self.photo_id = Some(photo_id);
        self.generation = self.generation.wrapping_add(1);
        self.image = None;
        self.texture = None;
        self.reveal = None;
        self.reveal_image = None;
        self.fade_start = None;
        self.display_aspect = None;
        self.linear = None;
        self.tone_inflight = false;
        self.tone_dirty = false;
        self.balanced_cache = None;
        self.curved_cache = None;
        if let Some(b) = &self.gpu_backend
            && let Ok(mut g) = b.lock() {
                g.invalidate();
            }
        self.loading = false;
        self.settled = true;
        self.demosaic_progress = None;
        self.status = Some(message);
    }

    pub fn clear(&mut self) {
        self.photo_id = None;
        self.generation = self.generation.wrapping_add(1);
        self.image = None;
        self.texture = None;
        self.reveal = None;
        self.reveal_image = None;
        self.fade_start = None;
        self.display_aspect = None;
        self.linear = None;
        self.tone_inflight = false;
        self.tone_dirty = false;
        self.balanced_cache = None;
        self.curved_cache = None;
        if let Some(b) = &self.gpu_backend
            && let Ok(mut g) = b.lock() {
                g.invalidate();
            }
        self.status = None;
        self.loading = false;
        self.settled = false;
        self.demosaic_progress = None;
    }

    /// Drain background results. Call once per frame.
    pub fn pump(&mut self, ctx: &egui::Context) {
        let mut need_repaint = false;
        while let Ok(r) = self.rx.try_recv() {
            if r.generation != self.generation || Some(r.photo_id) != self.photo_id {
                continue;
            }
            need_repaint = true;
            match r.kind {
                ResultKind::Placeholder(img) => {
                    // Never clobber an in-progress demosaic reveal or final image.
                    if self.fading() {
                        continue;
                    }
                    let replace = match &self.image {
                        None => true,
                        Some(cur) => !cur.source.is_final(),
                    };
                    if replace {
                        self.lock_aspect(img.width, img.height);
                        self.set_base(ctx, img);
                        if self.loading {
                            self.status = Some("Loading…".into());
                        }
                    }
                }
                ResultKind::DemosaicProgress(p) => {
                    self.demosaic_progress = Some(p.clamp(0.0, 1.0));
                    self.status = Some("Demosaicing…".into());
                }
                ResultKind::LinearReady {
                    linear,
                    display,
                    tone_used,
                } => {
                    self.linear = Some(linear);
                    self.demosaic_progress = None;
                    let has_placeholder = self.texture.is_some()
                        && matches!(
                            self.image.as_ref().map(|i| i.source),
                            Some(
                                PreviewSource::CachedThumb
                                    | PreviewSource::Embedded
                                    | PreviewSource::CachedPreview
                            )
                        );
                    // Always re-lock aspect from the demosaic: it is the
                    // authoritative source of the linear's aspect. The
                    // placeholder may be the un-oriented library thumb
                    // (cached at import time) which would set a wrong
                    // aspect and stretch the demosaic into the rect.
                    self.lock_aspect(display.width, display.height);
                    if has_placeholder {
                        // Keep thumb as base; fade demosaic in on top.
                        // Base texture is NOT replaced → no pop/flash.
                        self.begin_reveal(ctx, display);
                    } else {
                        self.set_base(ctx, display);
                    }
                    self.loading = false;
                    self.settled = true;
                    self.status = None;
                    if !self.tone.approx_eq(&tone_used) {
                        self.tone_dirty = true;
                        if !self.fading() {
                            self.schedule_tone();
                        }
                    }
                }
                ResultKind::LinearFailed { error, fallback } => {
                    self.linear = None;
                    self.demosaic_progress = None;
                    self.loading = false;
                    self.settled = true;
                    if let Some(img) = fallback {
                        let has_placeholder = self.texture.is_some()
                            && matches!(
                                self.image.as_ref().map(|i| i.source),
                                Some(
                                    PreviewSource::CachedThumb
                                        | PreviewSource::Embedded
                                        | PreviewSource::CachedPreview
                                )
                            );
                        if self.display_aspect.is_none() {
                            self.lock_aspect(img.width, img.height);
                        }
                        if has_placeholder {
                            self.begin_reveal(ctx, img);
                        } else {
                            self.set_base(ctx, img);
                        }
                        self.status = None;
                    } else if self.image.is_none() {
                        self.status = Some(error);
                    } else {
                        self.status = None;
                    }
                }
                ResultKind::Tone {
                    tone_gen,
                    img,
                    balanced_rgb,
                    balanced_w,
                    balanced_h,
                    curved_rgb,
                } => {
                    if self.fading() {
                        self.tone_inflight = false;
                        self.tone_dirty = true;
                    } else if tone_gen == self.tone_gen {
                        self.tone_inflight = false;
                        if let Some(rgb) = balanced_rgb {
                            self.balanced_cache = Some(StageCache {
                                params_hash: hash_balance(
                                    self.tone.exposure,
                                    self.tone.temp,
                                    self.tone.tint,
                                ),
                                rgb,
                                width: balanced_w,
                                height: balanced_h,
                                cached_at: Instant::now(),
                            });
                        }
                        if let Some(rgb) = curved_rgb {
                            self.curved_cache = Some(StageCache {
                                params_hash: hash_curves(
                                    self.tone.contrast,
                                    self.tone.highlights,
                                    self.tone.shadows,
                                    self.tone.whites,
                                    self.tone.blacks,
                                ),
                                rgb,
                                width: balanced_w,
                                height: balanced_h,
                                cached_at: Instant::now(),
                            });
                        }
                        self.set_base(ctx, img);
                    }
                    self.try_spawn_tone();
                }
            }
        }

        // Commit demosaic base once the reveal fade finishes.
        if let Some(start) = self.fade_start {
            if start.elapsed().as_secs_f32() >= FADE_SECS {
                self.commit_reveal();
                if self.tone_dirty && self.linear.is_some() {
                    self.schedule_tone();
                }
            }
            need_repaint = true;
        }

        if need_repaint {
            ctx.request_repaint();
        }
        if self.loading
            || self.tone_inflight
            || self.fading()
            || self.demosaic_progress.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn lock_aspect(&mut self, w: u32, h: u32) {
        if w > 0 && h > 0 {
            self.display_aspect = Some(w as f32 / h as f32);
        }
    }

    fn upload(&self, ctx: &egui::Context, img: &PreviewImage, tag: &str) -> egui::TextureHandle {
        let color = egui::ColorImage::from_rgba_unmultiplied(
            [img.width as usize, img.height as usize],
            &img.rgba,
        );
        let name = format!(
            "develop-{}-{}-{}-{}-{}",
            self.photo_id.unwrap_or(0),
            tag,
            self.generation,
            self.tone_gen,
            img.width
        );
        ctx.load_texture(name, color, egui::TextureOptions::LINEAR)
    }

    /// Replace the committed base immediately (no crossfade).
    fn set_base(&mut self, ctx: &egui::Context, img: PreviewImage) {
        self.reveal = None;
        self.reveal_image = None;
        self.fade_start = None;
        self.texture = Some(self.upload(ctx, &img, "base"));
        self.image = Some(img);
    }

    /// Start fading demosaic in on top of the existing thumbnail base.
    /// The base texture is left untouched for the whole fade.
    fn begin_reveal(&mut self, ctx: &egui::Context, demosaic: PreviewImage) {
        let tex = self.upload(ctx, &demosaic, "reveal");
        self.reveal = Some(tex);
        self.reveal_image = Some(demosaic);
        self.fade_start = Some(Instant::now());
    }

    /// Promote the reveal (demosaic) to the committed base and drop the thumb.
    fn commit_reveal(&mut self) {
        if let Some(tex) = self.reveal.take() {
            self.texture = Some(tex);
        }
        if let Some(img) = self.reveal_image.take() {
            self.image = Some(img);
        }
        self.fade_start = None;
    }

    pub fn texture(&self) -> Option<&egui::TextureHandle> {
        self.texture.as_ref()
    }

    /// Demosaic texture being revealed (fades in over the thumb base).
    pub fn reveal_texture(&self) -> Option<&egui::TextureHandle> {
        self.reveal.as_ref()
    }

    /// Locked display aspect (width/height) for stable layout.
    pub fn display_aspect(&self) -> Option<f32> {
        self.display_aspect.or_else(|| {
            let img = self.image.as_ref()?;
            if img.height == 0 {
                return None;
            }
            Some(img.width as f32 / img.height as f32)
        })
    }

    /// Reveal (demosaic) opacity: `0` at fade start → `1` when finished.
    /// `None` if no crossfade is active.
    pub fn reveal_alpha(&self) -> Option<f32> {
        let start = self.fade_start?;
        self.reveal.as_ref()?;
        let t = (start.elapsed().as_secs_f32() / FADE_SECS).clamp(0.0, 1.0);
        // Smoothstep — soft ease in/out, starts at 0 so first frame is pure thumb.
        Some(t * t * (3.0 - 2.0 * t))
    }

    /// Thumbnail opacity if drawn on top of demosaic (1 − reveal).
    /// Prefer drawing thumb base + demosaic reveal instead.
    pub fn overlay_alpha(&self) -> Option<f32> {
        Some(1.0 - self.reveal_alpha()?)
    }

    pub fn overlay_texture(&self) -> Option<&egui::TextureHandle> {
        self.reveal_texture()
    }

    pub fn underlay_texture(&self) -> Option<&egui::TextureHandle> {
        self.reveal_texture()
    }

    pub fn fade_progress(&self) -> Option<f32> {
        self.reveal_alpha()
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Demosaic stage progress (`0.0..=1.0`) while rawler is running.
    /// `None` when idle, cache-hit, or finished.
    pub fn demosaic_progress(&self) -> Option<f32> {
        self.demosaic_progress
    }

    pub fn source(&self) -> Option<PreviewSource> {
        self.image.as_ref().map(|i| i.source)
    }
}

fn load_cached_thumb(catalog_dir: &std::path::Path, photo_id: i64) -> Option<PreviewImage> {
    let bytes = thumbnail_cache::load_thumbnail(catalog_dir, photo_id)?;
    if bytes.rgba.is_empty() || bytes.width == 0 || bytes.height == 0 {
        return None;
    }
    Some(PreviewImage {
        width: bytes.width,
        height: bytes.height,
        rgba: bytes.rgba,
        source: PreviewSource::CachedThumb,
    })
}

/// Longest edge in physical pixels when fitting `img_w×img_h` into `avail` points.
fn fit_view_max_dim(avail: egui::Vec2, ppp: f32, img_w: u32, img_h: u32) -> u32 {
    if img_w == 0 || img_h == 0 || avail.x <= 0.0 || avail.y <= 0.0 {
        return 1;
    }
    let scale = (avail.x / img_w as f32).min(avail.y / img_h as f32);
    let display = egui::vec2(img_w as f32 * scale, img_h as f32 * scale);
    let phys = display * ppp;
    phys.x
        .max(phys.y)
        .ceil()
        .clamp(1.0, PREVIEW_MAX_DIM as f32) as u32
}

fn dim_changed_enough(prev: u32, next: u32) -> bool {
    let diff = prev.abs_diff(next);
    diff >= 16 || diff as f32 / prev.max(1) as f32 >= 0.05
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::develop::LinearPreview;

    /// Helper: build a `DevelopPreview` with the given view dim and
    /// dragging flag, no linear buffer attached (so `tone_max_dim`
    /// ignores the linear clamp).
    fn make(view_max_dim: u32, dragging: bool) -> DevelopPreview {
        let mut p = DevelopPreview::default();
        p.view_max_dim = view_max_dim;
        p.dragging = dragging;
        p
    }

    #[test]
    fn render_max_dim_is_full_when_idle() {
        let p = make(1500, false);
        assert_eq!(p.render_max_dim(), 1500);
    }

    #[test]
    fn render_max_dim_halves_while_dragging() {
        let p = make(1500, true);
        assert_eq!(p.render_max_dim(), 1500 / DRAG_QUALITY_DIVISOR);
    }

    #[test]
    fn render_max_dim_clamps_to_min_when_dragging() {
        let p = make(40, true);
        assert_eq!(p.render_max_dim(), DRAG_QUALITY_MIN_DIM);
    }

    #[test]
    fn render_max_dim_drops_back_to_full_after_drag_stops() {
        let mut p = make(1500, true);
        assert_eq!(p.render_max_dim(), 750);
        p.dragging = false;
        assert_eq!(p.render_max_dim(), 1500);
    }

    #[test]
    fn render_max_dim_caps_at_linear_buffer_size() {
        let mut p = make(2048, false);
        p.linear = Some(Arc::new(LinearPreview {
            width: 1024,
            height: 768,
            rgb: vec![0.0; 1024 * 768 * 3],
        }));
        // 2048 > 1024, so tone_max_dim clamps to 1024.
        assert_eq!(p.render_max_dim(), 1024);
        // Drag halves it.
        p.dragging = true;
        assert_eq!(p.render_max_dim(), 1024 / DRAG_QUALITY_DIVISOR);
    }
}
