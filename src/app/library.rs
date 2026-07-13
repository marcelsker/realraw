//! The main library page: a thumbnail grid of every photo in the
//! catalog, using the same card style as the import dialog but
//! without selection or "in catalog" hints.
//!
//! Thumbnails are loaded lazily by short-lived worker threads
//! (same pattern as the import dialog). Per-photo state is keyed
//! by `Photo::id`, not by index, so a refresh in the middle of
//! loading (e.g. when the import dialog transitions to Done and
//! bumps the catalog file's mtime) doesn't wipe in-flight work.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::thread;

use eframe::egui;
use time::OffsetDateTime;

use std::sync::Arc;

use crate::catalog::thumbnail_cache;
use crate::catalog::{Catalog, Photo};
use crate::photo_ops::RemoveDialog;
use crate::task::TaskManager;
use crate::thumb_grid::{self, GridItem, ThumbCardConfig, ThumbnailBytes};

/// Maximum number of outstanding thumbnail requests at any time.
/// Keeps the disk + decode load under control on big libraries.
const MAX_INFLIGHT_THUMBS: usize = 16;
/// Maximum times we'll retry a failed thumbnail before giving up
/// and showing the error permanently.
const MAX_THUMB_RETRIES: u32 = 3;
/// Vertical scroll area height for the library grid.
const SCROLL_MAX_HEIGHT: f32 = 6_000.0;

/// Group consecutive photos by calendar day. Returns groups in
/// display order, each with a formatted date label.
fn group_by_date(photos: &[Photo]) -> Vec<(String, Vec<&Photo>)> {
    let mut groups: Vec<(String, Vec<&Photo>)> = Vec::new();

    for photo in photos {
        let ts = photo.date_taken.unwrap_or(photo.imported_at);
        let dt = OffsetDateTime::from_unix_timestamp(ts).unwrap();
        let label = format!("{} {}, {}", dt.month(), dt.day(), dt.year());

        if groups.last().map(|(l, _)| l.as_str()) != Some(label.as_str()) {
            groups.push((label, Vec::new()));
        }
        groups.last_mut().unwrap().1.push(photo);
    }

    groups
}

/// Render an inline divider with the date label embedded in a
/// horizontal line: ──── July 1, 2026 ────
fn render_date_divider(ui: &mut egui::Ui, label: &str) {
    let height = 24.0;
    let (rect, _response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );

    let painter = ui.painter();
    let center_y = rect.center().y;
    let visuals = ui.style().visuals.clone();
    let line_color = visuals.widgets.noninteractive.bg_stroke.color;
    let text_color = visuals.weak_text_color();
    let font_id = egui::FontId::proportional(12.0);

    let galley = painter.layout_no_wrap(label.to_string(), font_id.clone(), text_color);
    let text_w = galley.size().x;
    let text_h = galley.size().y;
    let pad = 8.0;
    let line_y = center_y.floor() + 0.5;

    // Left line segment
    painter.line_segment(
        [
            egui::pos2(rect.min.x, line_y),
            egui::pos2(rect.min.x + (rect.width() - text_w) / 2.0 - pad, line_y),
        ],
        egui::Stroke::new(1.0, line_color),
    );

    // Right line segment
    painter.line_segment(
        [
            egui::pos2(rect.min.x + (rect.width() + text_w) / 2.0 + pad, line_y),
            egui::pos2(rect.max.x, line_y),
        ],
        egui::Stroke::new(1.0, line_color),
    );

    // Background behind text masks the line
    let bg_fill = visuals.widgets.noninteractive.bg_fill;
    painter.rect_filled(
        egui::Rect::from_center_size(
            egui::pos2(rect.min.x + rect.width() / 2.0, center_y),
            egui::vec2(text_w + 4.0, text_h + 2.0),
        ),
        egui::CornerRadius::same(2),
        bg_fill,
    );

    // Draw text
    painter.text(
        egui::pos2(rect.min.x + rect.width() / 2.0, center_y),
        egui::Align2::CENTER_CENTER,
        label,
        font_id,
        text_color,
    );
}

/// State for the main library page.
pub struct LibraryPage {
    /// Photos currently displayed, ordered by `imported_at` desc.
    pub photos: Vec<Photo>,
    /// `true` while a background import is running. The empty-state
    /// message shows a spinner + "Importing Photos…" instead of
    /// the default "Library is empty" hint.
    pub importing: bool,
    /// Per-photo thumbnail state keyed by `Photo::id` (stable
    /// across refreshes). Holding the state in a `HashMap` means
    /// re-reading the catalog doesn't wipe in-flight work -- if a
    /// refresh happens while a thumb is loading, the result is
    /// still kept and reused.
    thumbs: HashMap<i64, ThumbState>,

    remove_dialog: RemoveDialog,
    /// GPU textures, keyed by `CacheKey` (the photo's database id
    /// in the library; the path-hash in the import dialog). Owned
    /// by the page so they live as long as the photos do.
    textures: Mutex<HashMap<thumb_grid::CacheKey, egui::TextureHandle>>,

    thumb_tx: Sender<ThumbResult>,
    thumb_rx: Receiver<ThumbResult>,
    inflight_thumbs: AtomicUsize,

    /// Last load error (e.g. catalog query failure), if any.
    last_error: Option<String>,

    /// Set to the photo id that was double-clicked this frame.
    /// The central panel reads this and navigates to Develop mode.
    pub activated_id: Option<i64>,
}

/// Per-photo thumbnail state held in the library's `HashMap`.
/// Keeping this stable across refreshes means the in-flight thumb
/// workers' results can land in the right slot even if the user
/// scrolls, the import dialog re-runs, or the catalog mtime ticks
/// during a bulk import.
#[derive(Default)]
struct ThumbState {
    bytes: Option<ThumbnailBytes>,
    error: Option<String>,
    /// `true` once we've sent the file's path to the thumbnail
    /// worker. Used to avoid spawning duplicate workers.
    requested: bool,
    /// Number of failed attempts so far. Once this reaches
    /// [`MAX_THUMB_RETRIES`] we stop retrying and show the error
    /// text permanently.
    attempts: u32,
}

struct ThumbResult {
    /// Photo id, not index. Stable across refreshes.
    photo_id: i64,
    result: Result<ThumbnailBytes, String>,
}

impl Default for LibraryPage {
    fn default() -> Self {
        let (thumb_tx, thumb_rx) = channel();
        Self {
            photos: Vec::new(),
            importing: false,
            thumbs: HashMap::new(),
            textures: Mutex::new(HashMap::new()),
            thumb_tx,
            thumb_rx,
            inflight_thumbs: AtomicUsize::new(0),
            last_error: None,
            remove_dialog: RemoveDialog::default(),
            activated_id: None,
        }
    }
}

impl LibraryPage {
    /// Reload the photo list from the catalog. Cheap when the catalog
    /// is already loaded; `limit` caps the number of rows (`None`
    /// means everything).
    ///
    /// Per-photo thumbnail state is preserved by id: an existing
    /// photo keeps its loaded thumbnail, a new photo gets a fresh
    /// slot. Removed photos have their state dropped on the next
    /// render.
    pub fn refresh(&mut self, catalog: &Catalog, limit: Option<i64>) {
        match catalog.list_photos(limit) {
            Ok(photos) => {
                self.last_error = None;
                self.photos = photos;
                // Allocate a fresh ThumbState for any new photo
                // id. Existing entries are untouched.
                for p in &self.photos {
                    self.thumbs.entry(p.id).or_default();
                }
                // Drop the per-photo state for photos that have
                // been removed from the catalog.
                let live: std::collections::HashSet<i64> =
                    self.photos.iter().map(|p| p.id).collect();
                self.thumbs.retain(|id, _| live.contains(id));
                // Drop cached textures for removed photos. The
                // texture map is keyed by `CacheKey`; we filter by
                // the underlying photo id via the `from_id` round
                // trip.
                let live_keys: std::collections::HashSet<thumb_grid::CacheKey> = live
                    .iter()
                    .map(|id| thumb_grid::CacheKey::from_id(*id))
                    .collect();
                self.textures
                    .lock()
                    .unwrap()
                    .retain(|key, _| live_keys.contains(key));
            }
            Err(e) => {
                self.last_error = Some(e.to_string());
            }
        }
    }

    /// Drain pending thumbnail results from background workers and
    /// update thumbnail_status in the catalog.
    pub fn pump_events(&mut self, catalog: &Catalog) {
        while let Ok(r) = self.thumb_rx.try_recv() {
            self.inflight_thumbs.fetch_sub(1, Ordering::Relaxed);
            if let Some(state) = self.thumbs.get_mut(&r.photo_id) {
                match r.result {
                    Ok(bytes) => {
                        state.bytes = Some(bytes);
                        state.error = None;
                        state.requested = true;
                        let key = thumb_grid::CacheKey::from_id(r.photo_id);
                        self.textures.lock().unwrap().remove(&key);
                        // Mark the thumbnail as ready in the DB so
                        // subsequent launches skip this photo.
                        if let Ok(conn) = catalog.pool().get() {
                            let _ = conn.execute(
                                "UPDATE photos SET thumbnail_status = 1 WHERE id = ?1",
                                [r.photo_id],
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "thumb failed for photo {} (attempt {}): {e}",
                            r.photo_id,
                            state.attempts + 1,
                        );
                        state.error = Some(e);
                        state.requested = false;
                        state.attempts = state.attempts.saturating_add(1);
                    }
                }
            }
        }
    }

    /// Low-priority background refresh: rebuild the library thumbnail from
    /// develop linear + tone and swap it in when ready. Keeps the old
    /// thumb visible until the new one arrives.
    pub fn schedule_developed_thumb_refresh(
        &self,
        catalog_dir: PathBuf,
        photo_id: i64,
        source_path: PathBuf,
        orientation: Option<i64>,
        tone: crate::develop::ToneParams,
        gpu: Option<std::sync::Arc<crate::gpu::GpuContext>>,
    ) {
        self.inflight_thumbs.fetch_add(1, Ordering::Relaxed);
        let tx = self.thumb_tx.clone();
        let spawn = thread::Builder::new()
            .name(format!("dev-thumb-{photo_id}"))
            .spawn(move || {
                // Yield so interactive develop-tone / import work stays snappy.
                thread::sleep(std::time::Duration::from_millis(75));
                // wgpu types are not UnwindSafe; AssertUnwindSafe keeps
                // the catch_unwind contract intact while letting the GPU
                // path through.
                let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    thumbnail_cache::regenerate_from_develop(
                        &catalog_dir,
                        photo_id,
                        &source_path,
                        orientation,
                        tone,
                        gpu,
                    )
                })) {
                    Ok(r) => r,
                    Err(_) => Err("developed thumbnail worker panicked".into()),
                };
                let _ = tx.send(ThumbResult { photo_id, result });
            });
        if spawn.is_err() {
            self.inflight_thumbs.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Render the library grid.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        catalog: Arc<Catalog>,
        task_manager: &mut TaskManager,
    ) -> Option<usize> {
        self.activated_id = None;
        self.pump_events(&catalog);

        if let Some(err) = &self.last_error {
            ui.colored_label(egui::Color32::LIGHT_RED, err);
            return None;
        }

        if self.photos.is_empty() {
            ui.vertical_centered(|ui| {
                if self.importing {
                    ui.add_space(ui.available_height() / 3.0);
                    ui.heading("Importing Photos...");
                } else {
                    ui.add_space(80.0);
                    ui.heading("Library is empty");
                    let arrow = egui_phosphor::variants::regular::ARROW_RIGHT;
                    ui.label(format!("Use File {arrow} Import Photos to add some."));
                }
            });
            return None;
        }

        // Pre-load cached thumbnails from disk before spawning
        // workers. This prevents the infinite spinner on restart
        // when the disk cache already has the thumbnail.
        self.preload_cached_thumbs(&catalog);

        // Request thumbnails for every photo that doesn't have one
        // yet. The `inflight_thumbs` counter already caps the
        // number of in-flight extractions at MAX_INFLIGHT_THUMBS,
        // so this just keeps the work queue topped up.
        let layout = thumb_grid::compute_grid(ui);
        self.request_thumbs(&catalog);

        let groups = group_by_date(&self.photos);

        let mut remove_ids = Vec::new();
        let scroll = egui::ScrollArea::vertical()
            .max_height(SCROLL_MAX_HEIGHT)
            .drag_to_scroll(false)
            .auto_shrink([false, false]);
        scroll.show(ui, |ui| {
            for (date_label, group_photos) in &groups {
                render_date_divider(ui, date_label);

                let mut items: Vec<GridItem> = group_photos
                    .iter()
                    .map(|p| {
                        let state = self.thumbs.get(&p.id);
                        let bytes = state.and_then(|s| s.bytes.clone());
                        let error = state.and_then(|s| s.error.clone());
                        GridItem {
                            id: Some(p.id),
                            full_path: p.path.clone(),
                            thumb_bytes: bytes,
                            thumb_error: error,
                            rect: egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::ZERO),
                            config: ThumbCardConfig {
                                cell_w: layout.cell_w,
                                in_catalog: false,
                                label_override: None,
                                selectable: false,
                                selected: false,
                                clickable: true,
                            },
                        }
                    })
                    .collect();

                let (group_removed, group_activated) = thumb_grid::show_thumb_rows(
                    ctx,
                    ui,
                    &mut items,
                    &self.textures,
                    |item| match item.id {
                        Some(id) => thumb_grid::CacheKey::from_id(id),
                        None => thumb_grid::CacheKey::from_path(&item.full_path),
                    },
                );
                remove_ids.extend(group_removed);
                if let Some(id) = group_activated.into_iter().next() {
                    self.activated_id = Some(id);
                }
            }
        });

        for id in remove_ids {
            if let Some(photo) = self.photos.iter().find(|p| p.id == id) {
                self.remove_dialog.request(id, &photo.path);
            }
        }

        // Show the remove-confirmation dialog if one is pending.
        // The dialog spawns a background task for the actual deletion.
        if self.remove_dialog.show(ctx, task_manager, catalog.clone()).unwrap_or(false) {
            self.refresh(&catalog, None);
        }

        None
    }

    /// Synchronously load cached thumbnails from disk for all photos
    /// marked `thumbnail_status == 1`. This prevents the infinite
    /// spinner on restart: the disk cache is checked before any
    /// worker is spawned.
    fn preload_cached_thumbs(&mut self, catalog: &Catalog) {
        let catalog_dir = catalog.dir();
        for p in &self.photos {
            if p.thumbnail_status != 1 {
                continue;
            }
            let state = match self.thumbs.get_mut(&p.id) {
                Some(s) => s,
                None => continue,
            };
            // Skip only when we already hold decoded pixels. After a
            // develop refresh, `bytes` is replaced in `pump_events`.
            if state.bytes.is_some() {
                continue;
            }
            if let Some(bytes) = thumbnail_cache::load_thumbnail(catalog_dir, p.id) {
                state.bytes = Some(bytes);
                // Drop any GPU texture so the new disk bytes re-upload.
                let key = thumb_grid::CacheKey::from_id(p.id);
                self.textures.lock().unwrap().remove(&key);
            }
        }
    }

    /// Spawn thumbnail workers for every photo that doesn't have
    /// a loaded thumbnail yet, up to `MAX_INFLIGHT_THUMBS` at a
    /// time. Skips photos that are already in flight or have
    /// exhausted [`MAX_THUMB_RETRIES`].
    ///
    /// Photos with a stored error are re-queued (up to the retry
    /// limit): the previous attempt may have failed for a transient
    /// reason (disk hiccup, file temporarily missing, ...) and we
    /// should give them a chance to resolve. Once the limit is
    /// reached the error is shown permanently.
    fn request_thumbs(&mut self, catalog: &Catalog) {
        let candidates: Vec<(i64, PathBuf)> = self
            .photos
            .iter()
            .filter_map(|p| {
                let state = self.thumbs.get(&p.id)?;
                if state.bytes.is_some()
                    || state.requested
                    || state.attempts >= MAX_THUMB_RETRIES
                {
                    return None;
                }
                Some((p.id, PathBuf::from(&p.path)))
            })
            .collect();

        let catalog_dir = catalog.dir().to_path_buf();
        for (id, path) in candidates {
            if self.inflight_thumbs.load(Ordering::Relaxed) >= MAX_INFLIGHT_THUMBS {
                break;
            }
            if let Some(state) = self.thumbs.get_mut(&id) {
                state.requested = true;
                state.error = None;
            }
            self.inflight_thumbs.fetch_add(1, Ordering::Relaxed);
            let tx = self.thumb_tx.clone();
            let catalog_dir = catalog_dir.clone();
            match thread::Builder::new()
                .name(format!("lib-thumb-{id}"))
                .spawn(move || {
                    let result = match std::panic::catch_unwind(|| {
                        thumbnail_cache::get_or_generate(&catalog_dir, id, &path)
                    }) {
                        Ok(r) => r,
                        Err(_) => Err("thumbnail worker panicked".to_string()),
                    };
                    let _ = tx.send(ThumbResult { photo_id: id, result });
                }) {
                Ok(_) => {}
                Err(_) => {
                    // Thread spawn failed (e.g. system out of threads).
                    // Reset state so the photo can be retried later.
                    if let Some(state) = self.thumbs.get_mut(&id) {
                        state.requested = false;
                    }
                    self.inflight_thumbs.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_library_renders_no_photos() {
        let page = LibraryPage::default();
        assert!(page.photos.is_empty());
        assert!(page.thumbs.is_empty());
    }

    /// Refreshing the library with a superset of the existing
    /// photos must not wipe per-photo state: an existing photo's
    /// loaded thumbnail should survive the refresh.
    #[test]
    fn refresh_preserves_per_photo_state() {
        use crate::catalog::Photo;
        let mut page = LibraryPage {
            photos: vec![
                Photo {
                    id: 1,
                    path: "/a/a.jpg".into(),
                    ..Default::default()
                },
                Photo {
                    id: 2,
                    path: "/a/b.jpg".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        for p in &page.photos {
            page.thumbs.insert(
                p.id,
                ThumbState {
                    requested: true,
                    ..Default::default()
                },
            );
        }

        // Simulate a refresh that adds a new photo (id 3) at the
        // top, keeping the existing two.
        page.photos.insert(
            0,
            Photo {
                id: 3,
                path: "/a/c.jpg".into(),
                ..Default::default()
            },
        );
        for p in &page.photos {
            page.thumbs
                .entry(p.id)
                .or_default();
        }

        // All three should be present; the new one is fresh, the
        // existing two still have `requested = true`.
        assert_eq!(page.thumbs.len(), 3);
        assert!(page.thumbs.get(&1).unwrap().requested);
        assert!(page.thumbs.get(&2).unwrap().requested);
        assert!(!page.thumbs.get(&3).unwrap().requested);
    }

    /// Refreshing the library with a subset must drop the
    /// removed photos' state.
    #[test]
    fn refresh_drops_removed_photos() {
        use crate::catalog::Photo;
        let mut page = LibraryPage {
            photos: vec![
                Photo {
                    id: 1,
                    path: "/a/a.jpg".into(),
                    ..Default::default()
                },
                Photo {
                    id: 2,
                    path: "/a/b.jpg".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        for p in &page.photos {
            page.thumbs.insert(
                p.id,
                ThumbState {
                    requested: true,
                    ..Default::default()
                },
            );
        }

        // Drop photo id 1.
        page.photos.remove(0);
        let live: std::collections::HashSet<i64> = page.photos.iter().map(|p| p.id).collect();
        page.thumbs.retain(|id, _| live.contains(id));

        assert_eq!(page.thumbs.len(), 1);
        assert!(page.thumbs.contains_key(&2));
        assert!(!page.thumbs.contains_key(&1));
    }

    /// A failed thumbnail extraction must leave the photo in a
    /// state that `request_thumbs` can re-pick on the next pass.
    /// Otherwise the cell would sit at the spinner forever even
    /// though `inflight_thumbs` has dropped to 0.
    #[test]
    fn errored_thumb_can_be_retried() {
        use crate::catalog::Catalog;
        use crate::catalog::Photo;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let cat = Catalog::create(&dir.path().join("t.sqlite")).unwrap();
        let mut page = LibraryPage {
            photos: vec![Photo {
                id: 1,
                path: "/a/a.jpg".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        page.thumbs.insert(
            1,
            ThumbState {
                requested: true,
                ..Default::default()
            },
        );

        // Simulate a worker reporting failure.
        let (tx, rx) = std::sync::mpsc::channel();
        page.thumb_rx = rx;
        tx.send(ThumbResult {
            photo_id: 1,
            result: Err("boom".to_string()),
        })
        .unwrap();
        page.pump_events(&cat);

        // After the error: state has the error message, and both
        // `requested` and `bytes` are reset so a retry is possible.
        let state = page.thumbs.get(&1).unwrap();
        assert!(state.bytes.is_none());
        assert!(state.error.is_some());
        assert!(
            !state.requested,
            "errored cell must be re-queueable"
        );
    }

    /// A successful delivery after a prior error must clear the
    /// error and mark the photo as done.
    #[test]
    fn success_after_error_clears_error() {
        use crate::catalog::Catalog;
        use crate::catalog::Photo;
        use crate::thumb_grid::ThumbnailBytes;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let cat = Catalog::create(&dir.path().join("t.sqlite")).unwrap();
        let mut page = LibraryPage {
            photos: vec![Photo {
                id: 7,
                path: "/a/g.jpg".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        page.thumbs.insert(
            7,
            ThumbState {
                error: Some("old failure".into()),
                requested: true,
                ..Default::default()
            },
        );

        let (tx, rx) = std::sync::mpsc::channel();
        page.thumb_rx = rx;
        tx.send(ThumbResult {
            photo_id: 7,
            result: Ok(ThumbnailBytes {
                width: 1,
                height: 1,
                rgba: vec![0, 0, 0, 255],
            }),
        })
        .unwrap();
        page.pump_events(&cat);

        let state = page.thumbs.get(&7).unwrap();
        assert!(state.bytes.is_some());
        assert!(state.error.is_none());
    }
}
