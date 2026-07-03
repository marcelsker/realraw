//! Top-level application state and egui integration.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::app::library::LibraryPage;
use crate::catalog::{Catalog, Counts};
use crate::import::{ImportDialog, ImportSummary, dialog::Phase as DialogPhase};
use crate::task::{TaskManager, TaskSnapshot, TaskStatus};

type StartupResult = Option<(PathBuf, Catalog)>;

/// Top-level application state. Owned by eframe's run loop and rendered once
/// per frame via the [`eframe::App`] impl below.
pub struct App {
    /// Show the "About" modal.
    pub show_about: bool,

    /// Background task manager - owns every running / queued task.
    pub task_manager: TaskManager,

    /// Snapshot of the manager taken on the last frame; rendered this frame.
    pub last_snapshot: TaskSnapshot,
    
    /// Counter used to label successive demo batches.
    pub next_demo_id: u32,
    
    /// Whether the bottom dropdown panel is currently open.
    pub tasks_open: bool,
    
    /// When the most recent batch of tasks finished. Drives the
    /// "stay visible for 1s after done" grace period on the badge.
    pub all_done_at: Option<Instant>,
    
    /// Currently open catalog, or `None` if open failed.
    pub catalog: Option<Arc<Catalog>>,
    
    /// Last known row counts, refreshed each frame.
    pub catalog_counts: Option<Counts>,
    
    /// Last error from the catalog layer, surfaced in the status bar.
    pub catalog_error: Option<String>,
    
    /// The in-window import dialog, when open. Drop to close.
    pub import_dialog: Option<ImportDialog>,
    
    /// The library page: thumbnail grid of every photo in the catalog.
    pub library: LibraryPage,
    
    /// mtime (unix milliseconds) of the catalog file the last time
    /// we refreshed the library. `None` means "not yet refreshed".
    pub library_last_refresh_mtime_ms: Option<i64>,
    
    /// Set by the import dialog when an import batch finishes; the
    /// library checks this every frame and refreshes immediately
    /// instead of waiting for the mtime to drift forward.
    pub library_needs_refresh: bool,
    
    /// Last-seen phase of the import dialog. Used to detect the
    /// transition into [`DialogPhase::Done`] so we set
    /// `library_needs_refresh` *once*, on the transition, rather
    /// than every frame while the dialog stays in Done.
    pub last_dialog_phase: Option<DialogPhase>,
    
    /// Receiver for the import batch summary. Held after the dialog
    /// closes so we can defer the library refresh until the background
    /// import tasks actually finish writing to the catalog.
    pub(crate) import_summary_rx: Option<std::sync::mpsc::Receiver<ImportSummary>>,

    /// Logo texture for the About dialog.
    pub(crate) logo: Option<egui::TextureHandle>,

    /// Whether to show the first-launch setup dialog.
    pub show_setup_dialog: bool,

    /// Whether the "Quit realraw?" confirmation modal is open.
    pub show_close_dialog: bool,

    /// True after the user confirmed quit; suppresses the dialog logic so the
    /// pending `ViewportCommand::Close` is not cancelled by a follow-up frame.
    pub closing: bool,
    
    /// Collection name entered in the setup dialog.
    pub setup_name: String,
    
    /// Directory chosen in the setup dialog.
    pub setup_dir: PathBuf,
    
    /// Last error from catalog creation in the setup dialog.
    pub setup_error: Option<String>,

    /// Receiver for the background startup check result.
    startup_rx: Option<Receiver<StartupResult>>,

    /// Active toast notifications.
    pub toasts: crate::app::toasts::Toasts,
}

impl Default for App {
    fn default() -> Self {
        // Spawn a background thread to find + open the catalog so the
        // window renders immediately even when the catalog file is on
        // a slow filesystem (e.g. iCloud).
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("startup-catalog".into())
            .spawn(move || {
                // Prefer the last-loaded path, then the default.
                let result = Catalog::load_last_path()
                    .and_then(|p| Catalog::open_existing(&p).ok().map(|c| (p, c)))
                    .or_else(|| {
                        Catalog::default_path().ok()
                            .and_then(|p| Catalog::open_existing(&p).ok().map(|c| (p, c)))
                    });
                let _ = tx.send(result);
            })
            .expect("spawn startup-catalog");

        Self {
            show_about: false,
            task_manager: TaskManager::new().set_max_concurrency(4),
            last_snapshot: TaskSnapshot::default(),
            next_demo_id: 1,
            tasks_open: false,
            all_done_at: None,
            catalog: None,
            catalog_counts: None,
            catalog_error: None,
            import_dialog: None,
            library: LibraryPage::default(),
            library_last_refresh_mtime_ms: None,
            library_needs_refresh: false,
            last_dialog_phase: None,
            import_summary_rx: None,
            logo: None,
            show_setup_dialog: false,
            show_close_dialog: false,
            closing: false,
            setup_name: String::new(),
            setup_dir: PathBuf::new(),
            setup_error: None,
            startup_rx: Some(rx),
            toasts: crate::app::toasts::Toasts::default(),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain the startup check result (runs in a background thread).
        if let Some(rx) = &self.startup_rx
            && let Ok(result) = rx.try_recv()
        {
            self.startup_rx = None;
            match result {
                Some((path, cat)) => {
                    Catalog::save_last_path(&path);
                    let counts = cat.counts().ok();
                    self.catalog = Some(Arc::new(cat));
                    self.catalog_counts = counts;
                    self.catalog_error = None;
                    if let Some(cat) = self.catalog.as_ref() {
                        self.library.refresh(cat, None);
                    }
                    self.library_last_refresh_mtime_ms = self
                        .catalog
                        .as_ref()
                        .and_then(|c| std::fs::metadata(c.path()).ok())
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as i64);
                }
                None => {
                    let dir = directories::UserDirs::new()
                        .and_then(|u| u.picture_dir().map(|p| p.to_path_buf()))
                        .unwrap_or_else(|| PathBuf::from("."));
                    self.show_setup_dialog = true;
                    self.setup_name = "realraw".to_string();
                    self.setup_dir = dir;
                }
            }
        }

        // Drain background progress into the manager every frame.
        self.task_manager.sync();
        self.last_snapshot = self.task_manager.snapshot();

        // Counters used by both the menubar badge and the bottom panel.
        let total = self.last_snapshot.tasks.len();
        let running = self
            .last_snapshot
            .tasks
            .iter()
            .filter(|t| matches!(t.status(), TaskStatus::Running))
            .count();
        let has_running = running > 0;

        // Overall progress across every task. Smooth (moves with each
        // progress sample) and reaches 1.0 only when the last task finishes.
        let overall_progress = if total == 0 {
            0.0
        } else {
            self.last_snapshot
                .tasks
                .iter()
                .map(|t| t.progress())
                .sum::<f32>()
                / total as f32
        };

        // Grace period: keep the badge visible for 1 second after everything
        // completes so the user sees the 100% bar settle.
        const BADGE_GRACE: Duration = Duration::from_secs(1);
        let now = Instant::now();
        if total == 0 || has_running {
            self.all_done_at = None;
        } else if self.all_done_at.is_none() {
            self.all_done_at = Some(now);
        }
        let in_grace = self
            .all_done_at
            .is_some_and(|t| now.duration_since(t) < BADGE_GRACE);
        let show_badge = total > 0 && (has_running || in_grace || self.tasks_open);

        crate::app::menubar::render(self, ctx, show_badge, overall_progress);
        crate::app::tasks_panel::render(self, ctx, has_running, running, total);
        crate::app::status_bar::render(self, ctx);
        crate::app::central::render(self, ctx);

        if self.show_setup_dialog {
            crate::app::setup_dialog::render(self, ctx);
        }

        if self.show_about {
            crate::app::about_dialog::render(self, ctx);
        }
        if let Some(dialog) = self.import_dialog.as_mut() {
            let catalog = self.catalog.clone();
            let should_close = dialog.show(ctx, catalog, &mut self.task_manager);
            if should_close {
                // Take the summary receiver before dropping the dialog.
                // The import runs in the background; we defer the
                // library refresh until the summary arrives.
                self.import_summary_rx = dialog.import_summary_rx.take();
                self.import_dialog = None;
                // If no import was started (user just closed the dialog),
                // refresh immediately.
                if self.import_summary_rx.is_none() {
                    self.library_needs_refresh = true;
                }
            }
        } else {
            self.last_dialog_phase = None;
        }
        // Check if a background import finished since the last frame.
        if let Some(rx) = &self.import_summary_rx
            && let Ok(_) = rx.try_recv()
        {
            self.import_summary_rx = None;
            self.library_needs_refresh = true;
        }

        if !self.closing {
            let close_requested = ctx.input(|i| i.viewport().close_requested());

            if close_requested && !self.show_close_dialog {
                self.show_close_dialog = true;
            }

            if self.show_close_dialog {
                crate::app::close_dialog::render(self, ctx);
            }
        }

        self.toasts.show(ctx);

        // Keep repainting while tasks are running (smooth bar) and during
        // the grace period (so the badge clears on time).
        if has_running {
            ctx.request_repaint_after(Duration::from_millis(50));
        } else if in_grace
            && let Some(t) = self.all_done_at
        {
            let remaining = BADGE_GRACE.saturating_sub(now.duration_since(t));
            ctx.request_repaint_after(remaining);
        }
    }
}
