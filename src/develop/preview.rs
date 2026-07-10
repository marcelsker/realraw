//! Background progressive preview loader for Develop mode.
//!
//! Stages:
//! 1. Placeholder (disk sRGB JPEG cache / library thumb / embedded JPEG)
//! 2. Linear demosaic → [`LinearPreview`]: load from disk linear cache if
//!    present, otherwise rawler demosaic + write cache
//! 3. Exposure tone (`× 2^EV` + sRGB) — re-run on slider changes with
//!    generation-based cancel / coalescing (uses the in-RAM linear buffer)

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;

use super::decode::{
    decode_embedded_preview, decode_raw_preview, PreviewImage, PreviewSource, PREVIEW_MAX_DIM,
};
use super::pipeline::{apply_exposure, develop_linear_with_progress, LinearPreview};
use crate::catalog::{preview_cache, thumbnail_cache};

/// Crossfade duration from thumbnail → demosaic (seconds).
const FADE_SECS: f32 = 0.0;

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
        /// Exposure EV used to build `display`.
        exposure_used: f32,
    },
    /// Linear develop failed; optional gamma-encoded fallback image.
    LinearFailed {
        error: String,
        fallback: Option<PreviewImage>,
    },
    /// Exposure re-render at current on-screen preview size.
    Tone {
        tone_gen: u64,
        img: PreviewImage,
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
    /// Linear demosaic buffer for the current photo (pre-exposure).
    linear: Option<Arc<LinearPreview>>,
    /// Current exposure in EV stops.
    exposure: f32,
    /// True while the exposure slider is being dragged.
    dragging: bool,
    /// Longest edge (physical pixels) of the on-screen preview; 0 = unknown.
    view_max_dim: u32,
    /// Bumped on every scheduled tone render.
    tone_gen: u64,
    /// A tone worker is currently running.
    tone_inflight: bool,
    /// Need another tone pass after the in-flight one finishes.
    tone_dirty: bool,
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
            exposure: 0.0,
            dragging: false,
            view_max_dim: 0,
            tone_gen: 0,
            tone_inflight: false,
            tone_dirty: false,
            tx,
            rx,
        }
    }
}

impl DevelopPreview {
    pub fn is_active_for(&self, photo_id: i64) -> bool {
        self.photo_id == Some(photo_id) && (self.loading || self.settled || self.image.is_some())
    }

    /// Start loading `photo_id` for interactive develop.
    ///
    /// Progressive:
    /// 1. Placeholder: library thumb (reflects develop edits) → develop
    ///    sRGB cache → embedded JPEG
    /// 2. Linear demosaic → RAM buffer (disk linear cache preferred)
    /// 3. Apply `exposure` → display texture
    pub fn open(
        &mut self,
        photo_id: i64,
        path: PathBuf,
        orientation: Option<i64>,
        catalog_dir: PathBuf,
        exposure: f32,
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
        self.exposure = exposure;
        self.dragging = false;
        // Keep view_max_dim across photo switches (panel size is stable).
        self.tone_gen = 0;
        self.tone_inflight = false;
        self.tone_dirty = false;
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
                // on develop edits — over the EV=0 develop preview cache,
                // which is otherwise stale after exposure changes.
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
                            develop_linear_with_progress(&path, orientation, &mut report);
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
                        let display = apply_exposure(&linear, exposure, dim);
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
                                exposure_used: exposure,
                            },
                        });

                        // Disk caches after the UI is unblocked.
                        if demosaic_report.is_some() {
                            if let Err(e) = preview_cache::save_linear(
                                &catalog_dir,
                                photo_id,
                                orientation,
                                &linear,
                            ) {
                                eprintln!(
                                    "linear cache save failed for photo {photo_id}: {e}"
                                );
                            }
                        }
                        // Refresh EV-current JPEG placeholder for next open.
                        let cache_img = apply_exposure(&linear, exposure, PREVIEW_MAX_DIM);
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

    /// Update exposure (EV). Re-tones at the current on-screen preview size.
    /// In-flight tone jobs are superseded via `tone_gen` (render cancelling).
    pub fn set_exposure(&mut self, exposure: f32, dragging: bool) {
        let changed = (self.exposure - exposure).abs() > 1e-6 || self.dragging != dragging;
        self.exposure = exposure;
        self.dragging = dragging;
        if !changed && !self.tone_dirty {
            return;
        }
        if self.linear.is_none() {
            return;
        }
        self.schedule_tone();
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

    fn fading(&self) -> bool {
        self.reveal.is_some()
    }

    fn schedule_tone(&mut self) {
        self.tone_gen = self.tone_gen.wrapping_add(1);
        self.tone_dirty = true;
        self.try_spawn_tone();
    }

    fn try_spawn_tone(&mut self) {
        if self.tone_inflight || !self.tone_dirty {
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
        let exposure = self.exposure;
        let max_dim = self.tone_max_dim();
        let tx = self.tx.clone();

        thread::Builder::new()
            .name("develop-tone".into())
            .spawn(move || {
                let img = apply_exposure(&linear, exposure, max_dim);
                let _ = tx.send(PreviewResult {
                    photo_id,
                    generation,
                    kind: ResultKind::Tone { tone_gen, img },
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
                    exposure_used,
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
                    if self.display_aspect.is_none() {
                        self.lock_aspect(display.width, display.height);
                    }
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
                    let need_ev_tone = (self.exposure - exposure_used).abs() > 1e-6;
                    if need_ev_tone {
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
                ResultKind::Tone { tone_gen, img } => {
                    self.tone_inflight = false;
                    // Never swap textures mid-crossfade.
                    if self.fading() {
                        self.tone_dirty = true;
                    } else if tone_gen == self.tone_gen {
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
        if self.reveal.is_none() {
            return None;
        }
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
