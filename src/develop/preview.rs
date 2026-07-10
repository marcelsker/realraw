//! Background progressive preview loader for Develop mode.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use eframe::egui;

use super::decode::{
    decode_embedded_preview, decode_raw_preview, PreviewImage, PreviewSource,
};

/// Result delivered from a background decode job.
struct PreviewResult {
    photo_id: i64,
    generation: u64,
    kind: ResultKind,
}

enum ResultKind {
    Embedded(PreviewImage),
    Final(Result<PreviewImage, String>),
}

/// Owns the develop preview texture and drives progressive RAW decode.
pub struct DevelopPreview {
    pub photo_id: Option<i64>,
    generation: u64,
    /// Latest decoded image (embedded or demosaic).
    image: Option<PreviewImage>,
    /// GPU texture for `image`.
    texture: Option<egui::TextureHandle>,
    /// Human-readable status while loading or on error.
    pub status: Option<String>,
    /// True while a background job for the current generation is in flight.
    loading: bool,
    /// True after a job for `photo_id` finished (success or hard failure).
    settled: bool,
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
            status: None,
            loading: false,
            settled: false,
            tx,
            rx,
        }
    }
}

impl DevelopPreview {
    /// `true` if this photo is already loaded, loading, or finished with an error.
    pub fn is_active_for(&self, photo_id: i64) -> bool {
        self.photo_id == Some(photo_id) && (self.loading || self.settled || self.image.is_some())
    }

    /// Start loading `photo_id` from `path`. Cancels prior work via generation.
    pub fn open(&mut self, photo_id: i64, path: PathBuf, orientation: Option<i64>) {
        if self.is_active_for(photo_id) {
            return;
        }

        self.photo_id = Some(photo_id);
        self.generation = self.generation.wrapping_add(1);
        self.image = None;
        self.texture = None;
        self.loading = true;
        self.settled = false;

        let job_gen = self.generation;
        let tx = self.tx.clone();

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
                // Phase 1: embedded JPEG for fast first paint.
                if let Ok(img) = decode_embedded_preview(&path, orientation) {
                    let _ = tx.send(PreviewResult {
                        photo_id,
                        generation: job_gen,
                        kind: ResultKind::Embedded(img),
                    });
                }

                // Phase 2: demosaic (or decoder RGB fallback).
                let final_result =
                    decode_raw_preview(&path, orientation).map_err(|e| e.to_string());
                let _ = tx.send(PreviewResult {
                    photo_id,
                    generation: job_gen,
                    kind: ResultKind::Final(final_result),
                });
            })
            .expect("spawn develop-preview");
    }

    /// Mark the current photo as failed without starting a decode job.
    pub fn fail(&mut self, photo_id: i64, message: String) {
        self.photo_id = Some(photo_id);
        self.generation = self.generation.wrapping_add(1);
        self.image = None;
        self.texture = None;
        self.loading = false;
        self.settled = true;
        self.status = Some(message);
    }

    /// Clear the current photo and any pending display.
    pub fn clear(&mut self) {
        self.photo_id = None;
        self.generation = self.generation.wrapping_add(1);
        self.image = None;
        self.texture = None;
        self.status = None;
        self.loading = false;
        self.settled = false;
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
                ResultKind::Embedded(img) => {
                    // Only apply embedded if we don't already have demosaic.
                    let replace = match &self.image {
                        None => true,
                        Some(cur) => cur.source != PreviewSource::Demosaic
                            && cur.source != PreviewSource::DecoderPreview,
                    };
                    if replace {
                        self.apply_image(ctx, img);
                        if self.loading {
                            self.status = Some("Loading…".into());
                        }
                    }
                }
                ResultKind::Final(Ok(img)) => {
                    self.apply_image(ctx, img);
                    self.loading = false;
                    self.settled = true;
                    self.status = None;
                }
                ResultKind::Final(Err(e)) => {
                    self.loading = false;
                    self.settled = true;
                    if self.image.is_none() {
                        self.status = Some(e);
                    } else {
                        // Keep embedded preview; clear loading status.
                        self.status = None;
                    }
                }
            }
        }
        if need_repaint {
            ctx.request_repaint();
        }
        if self.loading {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn apply_image(&mut self, ctx: &egui::Context, img: PreviewImage) {
        let color = egui::ColorImage::from_rgba_unmultiplied(
            [img.width as usize, img.height as usize],
            &img.rgba,
        );
        let name = format!(
            "develop-preview-{}-{:?}",
            self.photo_id.unwrap_or(0),
            img.source
        );
        self.texture = Some(ctx.load_texture(name, color, egui::TextureOptions::LINEAR));
        self.image = Some(img);
    }

    /// Texture to draw, if any.
    pub fn texture(&self) -> Option<&egui::TextureHandle> {
        self.texture.as_ref()
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn source(&self) -> Option<PreviewSource> {
        self.image.as_ref().map(|i| i.source)
    }
}
