//! In-window import dialog.
//!
//! ## Layout
//!
//! ```text
//! ┌────────────────────────────────────────────────┐
//! │ Import Photos   [Import All] [Import Selected] X│
//! ├────────────────────────────────────────────────┤
//! │ Source: /Users/sker/Pictures/                  │
//! │ 238 photos found, 12 already in catalog        │
//! ├────────────────────────────────────────────────┤
//! │ ┌──────┐ ┌──────┐ ┌──────┐ ┌──────┐ ┌──────┐  │
//! │ │ ☑ img│ │ ☑ img│ │ ☐ img│ │ ☑ img│ │ ☑ img│  │
//! │ │cr2_01│ │dng_01│ │jpg_01│ │arw_01│ │nef_01│  │
//! │ └──────┘ └──────┘ └──────┘ └──────┘ └──────┘  │
//! │ ┌──────┐ ┌──────┐ ┌──────┐                     │
//! │ │ ☑ img│ │ ☐ img│ │ ☑ img│                     │
//! │ │orf_01│ │pef_01│ │rw2_01│                     │
//! │ └──────┘ └──────┘ └──────┘                     │
//! └────────────────────────────────────────────────┘
//! ```
//!
//! ## Threading model
//!
//! * **Discovery** runs in a dedicated worker thread started by
//!   [`ImportDialog::begin_discovery`]. It walks the chosen paths and
//!   fills in [`ImportDialog::files`].
//! * **Thumbnails** are loaded on demand by short-lived worker threads
//!   (one per request). The result is sent back through a channel and
//!   stored on the dialog as RGBA bytes; the renderer converts them to
//!   an egui `TextureHandle` on the UI thread, cached per file index.
//! * **Import** is delegated to the
//!   [`crate::import::worker::import_batch`] which uses the shared
//!   [`crate::task::TaskManager`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use eframe::egui;

use crate::catalog::Catalog;
use crate::import::discovery::{DiscoveredFile, KNOWN_EXTENSIONS};
use crate::import::thumbnail::{extract_dialog_preview, Thumbnail};
use crate::import::worker::{import_batch, ImportFile, ImportSummary};
/// Maximum number of outstanding thumbnail requests.
const MAX_INFLIGHT_THUMBS: usize = 48;
/// Number of cell-rows of buffer ahead of the visible area that we keep
/// "queued for loading".
const ROW_LOOKAHEAD: usize = 2;

/// One per file: discovery result + per-row UI state.
pub struct DialogFile {
    pub path: PathBuf,
    pub selected: bool,
    pub already_in_catalog: bool,
    /// Raw RGBA bytes for the thumbnail (filled by the thumb worker).
    pub thumb_bytes: Option<ThumbnailBytes>,
    /// `true` once we've sent the file's path to the thumbnail worker.
    pub thumb_requested: bool,
    /// `true` once a thumbnail (or error) has been delivered.
    pub thumb_ready: bool,
    /// The most recent error from the thumbnail extractor, if any.
    pub thumb_error: Option<String>,
}

pub use crate::thumb_grid::ThumbnailBytes;

/// Current lifecycle phase of the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Nothing has been chosen yet.
    Empty,
    /// Walking the filesystem.
    Discovering,
    /// Discovery done; rendering grid.
    Browsing,
    /// User clicked "Import"; tasks are running.
    Importing,
    /// Import finished; showing summary.
    Done,
}

/// What the dialog is currently doing.
pub struct ImportDialog {
    pub phase: Phase,
    pub files: Vec<DialogFile>,
    pub extensions: Vec<String>,
    pub sources: Vec<PathBuf>,
    pub status: String,

    discovery_cancel: Arc<AtomicBool>,
    discovery_rx: Option<Receiver<DiscoveryEvent>>,

    thumb_tx: Sender<ThumbResult>,
    thumb_rx: Receiver<ThumbResult>,
    inflight_thumbs: usize,

    import_summary: Option<ImportSummary>,
    import_in_flight: bool,
    pub(crate) import_summary_rx: Option<Receiver<ImportSummary>>,

    /// Per-dialog texture cache. Keyed by file index. Dropped when the
    /// dialog closes.
    textures: Mutex<HashMap<crate::thumb_grid::CacheKey, egui::TextureHandle>>,
}

#[derive(Debug)]
enum DiscoveryEvent {
    Progress(String),
    Done(Vec<DiscoveredFile>),
    #[allow(dead_code)]
    Failed(String),
}

struct ThumbResult {
    index: usize,
    /// Path of the file whose thumb was extracted. Used to compute
    /// the cache key on delivery (so the GPU texture map stays
    /// consistent with the path-based key the renderer uses).
    path: PathBuf,
    result: Result<Thumbnail, String>,
}

impl Default for ImportDialog {
    fn default() -> Self {
        let (thumb_tx, thumb_rx) = channel();
        Self {
            phase: Phase::Empty,
            files: Vec::new(),
            extensions: KNOWN_EXTENSIONS.iter().map(|s| s.to_string()).collect(),
            sources: Vec::new(),
            status: String::new(),
            discovery_cancel: Arc::new(AtomicBool::new(false)),
            discovery_rx: None,
            thumb_tx,
            thumb_rx,
            inflight_thumbs: 0,
            import_summary: None,
            import_in_flight: false,
            import_summary_rx: None,
            textures: Mutex::new(HashMap::new()),
        }
    }
}

impl Drop for ImportDialog {
    fn drop(&mut self) {
        self.discovery_cancel.store(true, Ordering::Relaxed);
    }
}

impl ImportDialog {
    /// Begin discovery in a background thread. `sources` is the list of
    /// files / folders the user just picked.
    pub fn begin_discovery(
        &mut self,
        sources: Vec<PathBuf>,
        catalog: Option<Arc<Catalog>>,
    ) {
        // Cancel any in-flight discovery and drain stale events.
        self.discovery_cancel.store(true, Ordering::Relaxed);
        while self
            .discovery_rx
            .as_ref()
            .and_then(|r| r.try_recv().ok())
            .is_some()
        {}

        // Clear texture cache.
        self.textures.lock().unwrap().clear();

        self.sources = sources.clone();
        self.files.clear();
        self.status = format!("Scanning {} location(s)...", self.sources.len());
        self.phase = Phase::Discovering;

        let cancel = Arc::new(AtomicBool::new(false));
        self.discovery_cancel = cancel.clone();
        let (tx, rx) = channel();
        self.discovery_rx = Some(rx);

        let extensions = self.extensions.clone();
        thread::Builder::new()
            .name("import-discovery".into())
            .spawn(move || {
                run_discovery(sources, extensions, catalog, cancel, tx);
            })
            .expect("spawn discovery");
    }

    /// Pump background events (discovery + thumbnail results) into the
    /// dialog state. Must be called every frame the dialog is open.
    pub fn pump_events(&mut self) {
        // Discovery
        if let Some(rx) = &self.discovery_rx {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    DiscoveryEvent::Progress(s) => self.status = s,
                    DiscoveryEvent::Done(files) => {
                        self.files = files
                            .into_iter()
                            .map(|f| DialogFile {
                                path: f.path,
                                selected: !f.already_in_catalog,
                                already_in_catalog: f.already_in_catalog,
                                thumb_bytes: None,
                                thumb_requested: false,
                                thumb_ready: false,
                                thumb_error: None,
                            })
                            .collect();
                        let n = self.files.len();
                        let dupes =
                            self.files.iter().filter(|f| f.already_in_catalog).count();
                        self.status = format!("{n} photos found, {dupes} already in catalog");
                        self.phase = Phase::Browsing;
                    }
                    DiscoveryEvent::Failed(s) => {
                        self.status = format!("Discovery failed: {s}");
                        self.phase = Phase::Empty;
                    }
                }
            }
        }
        // Thumbnails
        while let Ok(r) = self.thumb_rx.try_recv() {
            self.inflight_thumbs = self.inflight_thumbs.saturating_sub(1);
            if let Some(f) = self.files.get_mut(r.index) {
                match r.result {
                    Ok(t) => {
                        f.thumb_error = None;
                        f.thumb_bytes = Some(ThumbnailBytes {
                            width: t.width,
                            height: t.height,
                            rgba: t.rgba,
                        });
                        // Drop any cached texture; it will be rebuilt
                        // next frame from the new bytes.
                        let key = crate::thumb_grid::CacheKey::from_path(
                            &r.path.to_string_lossy(),
                        );
                        self.textures.lock().unwrap().remove(&key);
                    }
                    Err(e) => {
                        eprintln!(
                            "thumb failed for {}: {e}",
                            r.path.display(),
                        );
                        f.thumb_error = Some(e);
                        // Critical: clear `thumb_requested` and
                        // `thumb_ready` so the next
                        // `request_visible_thumbs` pass picks this
                        // file up again. Without this, a failed
                        // extraction would pin the cell at the
                        // spinner forever (matching the library's
                        // bug, just in different shape).
                        f.thumb_requested = false;
                        f.thumb_ready = false;
                        // Don't set `thumb_ready = true` here -- the
                        // cell will stay in the "loading" state and
                        // get retried.
                        continue;
                    }
                }
                f.thumb_ready = true;
            }
        }
    }

    fn request_visible_thumbs(
        &mut self,
        first_visible: usize,
        last_visible: usize,
        cells_per_row: usize,
    ) {
        if self.phase != Phase::Browsing {
            return;
        }
        let row_lookahead_rows = ROW_LOOKAHEAD * cells_per_row;
        let end = (last_visible + row_lookahead_rows).min(self.files.len());
        for i in first_visible..end {
            if self.inflight_thumbs >= MAX_INFLIGHT_THUMBS {
                break;
            }
            let Some(f) = self.files.get(i) else {
                break;
            };
            if f.thumb_requested || f.thumb_ready {
                continue;
            }
            let path = f.path.clone();
            // Mark first to avoid double-spawn in the same frame.
            if let Some(f) = self.files.get_mut(i) {
                f.thumb_requested = true;
                // Clear any prior error so the cell goes back to
                // showing the spinner while we wait for the retry.
                f.thumb_error = None;
            }
            self.inflight_thumbs += 1;
            let tx = self.thumb_tx.clone();
            let result = thread::Builder::new()
                .name(format!("thumb-{i}"))
                .spawn(move || {
                    let result = match std::panic::catch_unwind(|| {
                        extract_dialog_preview(&path).map_err(|e| e.to_string())
                    }) {
                        Ok(r) => r,
                        Err(_) => Err("thumbnail worker panicked".to_string()),
                    };
                    let _ = tx.send(ThumbResult {
                        index: i,
                        path,
                        result,
                    });
                });
            if result.is_err() {
                // Spawn failed — reset state so the cell can try
                // again on the next frame.
                self.inflight_thumbs -= 1;
                if let Some(f) = self.files.get_mut(i) {
                    f.thumb_requested = false;
                }
            }
        }
    }

    /// Render the dialog. Returns `true` if the dialog should be closed.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        catalog: Option<Arc<Catalog>>,
        task_manager: &mut crate::task::TaskManager,
    ) -> bool {
                self.pump_events();

        let mut should_close = false;

        let response = egui::Modal::new(egui::Id::new("import_dialog"))
            .show(ctx, |ui| {
                ui.heading("Import Photos");
                ui.horizontal(|ui| {
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let close_icon = egui_phosphor::variants::regular::X;
                            if ui.button(close_icon.to_string()).on_hover_text("Close").clicked() {
                                should_close = true;
                            }
                        },
                    );
                });
                ui.separator();

                match self.phase {
                    Phase::Empty => {
                        ui.vertical_centered(|ui| {
                            ui.add_space(20.0);
                            let icon_f = egui_phosphor::variants::regular::FOLDER;
                            if ui.button(format!("{icon_f}  Select Folder")).clicked()
                                && let Some(p) = rfd::FileDialog::new().pick_folder()
                            {
                                self.sources = vec![p];
                                self.begin_discovery(self.sources.clone(), catalog.clone());
                            }
                            ui.add_space(8.0);
                            let icon_file = egui_phosphor::variants::regular::FILE;
                            if ui.button(format!("{icon_file}  Select Files")).clicked()
                            {
                                let exts: Vec<&str> = KNOWN_EXTENSIONS
                                    .iter()
                                    .map(|s| s.trim_start_matches('.'))
                                    .collect();
                                if let Some(paths) = rfd::FileDialog::new()
                                    .add_filter("Images", &exts)
                                    .pick_files()
                                {
                                    self.sources = paths;
                                    self.begin_discovery(self.sources.clone(), catalog.clone());
                                }
                            }
                        });
                    }
                    Phase::Discovering => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(&self.status);
                        });
                    }
                    Phase::Browsing => {
                        // Watch for the import-summary channel so we
                        // can move to the Done phase without blocking.
                        if self.import_in_flight
                            && let Some(rx) = &self.import_summary_rx
                            && let Ok(summary) = rx.try_recv()
                        {
                            self.import_in_flight = false;
                            self.import_summary = Some(summary);
                            should_close = true;
                        }
                        self.show_grid(ctx, ui);
                    }
                    Phase::Importing => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Importing...");
                        });
                        if let Some(rx) = &self.import_summary_rx
                            && let Ok(summary) = rx.try_recv()
                        {
                            self.import_in_flight = false;
                            self.import_summary = Some(summary);
                            should_close = true;
                        }
                    }
                    Phase::Done => {
                        if let Some(s) = &self.import_summary {
                            ui.label(format!(
                                "{} imported, {} duplicates, {} errors",
                                s.imported, s.skipped_duplicates, s.errors
                            ));
                            for line in &s.sample_errors {
                                ui.colored_label(egui::Color32::LIGHT_RED, line);
                            }
                        }
                        if ui.button("Close").clicked() {
                            should_close = true;
                        }
                    }
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if self.phase == Phase::Browsing || self.phase == Phase::Done {
                        let selected = self.files.iter().filter(|f| f.selected).count();
                        ui.label(format!("{} selected of {}", selected, self.files.len()));
                    }
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if self.phase == Phase::Browsing {
                                let any = self.files.iter().any(|f| f.selected);
                                if ui
                                    .add_enabled(any, egui::Button::new("Import Selected"))
                                    .clicked()
                                    && let Some(cat) = catalog.clone()
                                {
                                    let files: Vec<ImportFile> = self
                                        .files
                                        .iter()
                                        .map(|f| ImportFile {
                                            path: f.path.clone(),
                                            selected: f.selected,
                                        })
                                        .collect();
                                    self.import_summary_rx = Some(import_batch(
                                        task_manager,
                                        cat,
                                        files,
                                        "Import",
                                        None,
                                    ));
                                    should_close = true;
                                }
                                if ui.button("Select All").clicked() {
                                    for f in &mut self.files {
                                        f.selected = true;
                                    }
                                }
                                if ui.button("Select None").clicked() {
                                    for f in &mut self.files {
                                        f.selected = false;
                                    }
                                }
                            }
                        },
                    );
                });
            });

        if response.should_close() {
            should_close = true;
        }

        if should_close {
            self.discovery_cancel.store(true, Ordering::Relaxed);
        }

        should_close
    }

    fn show_grid(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let layout = crate::thumb_grid::compute_grid(ui);

        // Request thumbnails for every file, not just the visible
        // ones. The `inflight_thumbs` counter already caps the
        // number of in-flight extractions at MAX_INFLIGHT_THUMBS,
        // so this just keeps the work queue topped up. The user
        // expects every "..." cell to eventually resolve when the
        // queue is bounded -- not the first few rows only.
        self.request_visible_thumbs(0, self.files.len(), layout.cells_per_row);

        // Project each DialogFile into a GridItem with the right
        // card config. Selectable + selected mirrors the dialog's
        // per-file `selected` flag, but in-catalog rows are pinned
        // unselected so we never re-import them by accident.
        let mut items: Vec<crate::thumb_grid::GridItem> = self
            .files
            .iter()
            .map(|f| crate::thumb_grid::GridItem {
                id: None,
                full_path: f.path.to_string_lossy().into_owned(),
                thumb_bytes: f.thumb_bytes.clone(),
                thumb_error: f.thumb_error.clone(),
                config: crate::thumb_grid::ThumbCardConfig {
                    cell_w: layout.cell_w,
                    selectable: !f.already_in_catalog,
                    selected: f.selected,
                    in_catalog: f.already_in_catalog,
                    label_override: None,
                    selected_count: 0,
                },
                rect: egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::ZERO),
            })
            .collect();

        crate::thumb_grid::show_thumb_grid(
            ctx,
            ui,
            &mut items,
            &self.textures,
            420.0,
            |item| crate::thumb_grid::CacheKey::from_path(&item.full_path),
        );

        // Apply any click results back to the dialog state. The
        // grid helper can't mutate `self.files` directly (the
        // closure has its own copy), so we walk the items once more
        // and trust the index: a click on item `i` toggles
        // `self.files[i].selected`.
        for (i, item) in items.iter().enumerate() {
            if item.config.selected != self.files[i].selected
                && !self.files[i].already_in_catalog
            {
                self.files[i].selected = item.config.selected;
            }
        }
    }
}

fn run_discovery(
    sources: Vec<PathBuf>,
    extensions: Vec<String>,
    catalog: Option<Arc<Catalog>>,
    cancel: Arc<AtomicBool>,
    tx: Sender<DiscoveryEvent>,
) {
    use walkdir::WalkDir;

    let catalog_ref = catalog.as_deref();
    let existing = catalog_ref
        .map(|c| c.existing_paths().unwrap_or_default())
        .unwrap_or_default();
    let exts: std::collections::HashSet<String> =
        extensions.iter().map(|e| e.to_ascii_lowercase()).collect();

    let mut out: Vec<DiscoveredFile> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut count: usize = 0;

    for source in &sources {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if !source.exists() {
            continue;
        }
        if source.is_file() {
            if !seen.insert(source.clone()) {
                continue;
            }
            count += 1;
            let Some(ext) = source.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            let ext_with_dot = format!(".{}", ext.to_ascii_lowercase());
            if !exts.contains(&ext_with_dot) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(source) else {
                continue;
            };
            let already = existing.contains(&source.to_string_lossy().into_owned());
            out.push(DiscoveredFile {
                path: source.clone(),
                file_size: meta.len(),
                mtime: meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                extension: ext_with_dot,
                already_in_catalog: already,
            });
            continue;
        }
        for entry in WalkDir::new(source)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !seen.insert(path.to_path_buf()) {
                continue;
            }
            count += 1;
            if count.is_multiple_of(64) {
                let _ = tx.send(DiscoveryEvent::Progress(format!(
                    "Scanning... {count} files so far"
                )));
            }
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            let ext_with_dot = format!(".{}", ext.to_ascii_lowercase());
            if !exts.contains(&ext_with_dot) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(path) else {
                continue;
            };
            let already = existing.contains(&path.to_string_lossy().into_owned());
            out.push(DiscoveredFile {
                path: path.to_path_buf(),
                file_size: meta.len(),
                mtime: meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                extension: ext_with_dot,
                already_in_catalog: already,
            });
        }
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    let _ = tx.send(DiscoveryEvent::Done(out));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thumb_grid::fit_inside;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn new_dialog_is_in_empty_phase() {
        let d = ImportDialog::default();
        assert_eq!(d.phase, Phase::Empty);
        assert!(d.files.is_empty());
    }

    #[test]
    fn fit_inside_preserves_aspect_and_centers() {
        let outer = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 100.0));
        // 3:2 inside a square should fit to 100x66.66... centered.
        let r = fit_inside(outer, 300.0, 200.0);
        assert!((r.width() - 100.0).abs() < 0.01);
        assert!((r.height() - 100.0 * (2.0 / 3.0)).abs() < 0.01);
        // Centred horizontally; the inner rect is 100 wide inside a
        // 100-wide outer, so x must be 0.
        assert_eq!(r.min.x, 0.0);
        // And vertically there should be a band above and below.
        assert!(r.min.y > 0.0);
    }

    #[test]
    fn fit_inside_letterboxes_tall_sources() {
        let outer = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 100.0));
        // 1:1 inside a 3:2 frame: fits to 100x100 with horizontal bars.
        let r = fit_inside(outer, 200.0, 200.0);
        assert!((r.width() - 100.0).abs() < 0.01);
        assert!((r.height() - 100.0).abs() < 0.01);
        assert_eq!(r.min, egui::pos2(0.0, 0.0));
    }

    #[test]
    fn discovery_completes_and_populates_files() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.jpg");
        let b = dir.path().join("b.jpg");
        std::fs::File::create(&a)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        std::fs::File::create(&b)
            .unwrap()
            .write_all(b"x")
            .unwrap();

        let mut d = ImportDialog::default();
        d.begin_discovery(vec![dir.path().to_path_buf()], None);

        let mut found = Vec::new();
        for _ in 0..500 {
            d.pump_events();
            if d.phase == Phase::Browsing {
                found = d.files.iter().map(|f| f.path.clone()).collect();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(found.len(), 2, "got {found:?}");
    }

    /// An errored thumbnail must clear `thumb_requested` and
    /// `thumb_ready` so the cell gets retried on the next
    /// `request_visible_thumbs` pass. Otherwise the cell sits at
    /// the spinner forever, mirroring the library bug.
    #[test]
    fn errored_thumb_can_be_retried() {
        let mut d = ImportDialog::default();
        d.phase = Phase::Browsing;
        d.files = vec![DialogFile {
            path: std::path::PathBuf::from("/no/such/file.jpg"),
            selected: true,
            already_in_catalog: false,
            thumb_bytes: None,
            thumb_requested: true,
            thumb_ready: false,
            thumb_error: None,
        }];
        // Simulate a worker that errored: re-feed the channel with
        // an Err result for the file. `pump_events` should reset
        // `thumb_requested` and `thumb_ready` to `false` and store
        // the error message.
        d.thumb_tx
            .send(ThumbResult {
                index: 0,
                path: d.files[0].path.clone(),
                result: Err("boom".to_string()),
            })
            .unwrap();
        d.pump_events();
        assert!(d.files[0].thumb_error.is_some());
        assert!(!d.files[0].thumb_requested);
        assert!(!d.files[0].thumb_ready);

        // Now `request_visible_thumbs` should re-queue the file and
        // clear the error. The new worker would fail again (the
        // path doesn't exist), but the *state* must be re-queueable
        // -- the actual error is a separate concern.
        d.request_visible_thumbs(0, d.files.len(), 4);
        assert!(
            d.files[0].thumb_requested,
            "errored file should be re-queued"
        );
        assert!(
            d.files[0].thumb_error.is_none(),
            "error should be cleared on retry"
        );
        // Avoid actually spawning a worker for the bad path in the
        // test cleanup; the test harness would have tried to
        // extract_thumbnail on /no/such/file.jpg and recorded
        // another error.
        d.files[0].thumb_requested = false;
    }
}
