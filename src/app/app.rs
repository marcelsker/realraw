//! Top-level application state and egui integration.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use directories::UserDirs;

use eframe::egui;

use crate::app::library::LibraryPage;
use crate::catalog::{Catalog, Counts};
use crate::import::{ImportDialog, ImportSummary, dialog::Phase as DialogPhase};
use crate::task::{TaskCommand, TaskManager, TaskSnapshot, TaskStatus, TaskViewOptions};

/// Top-level application state. Owned by eframe's run loop and rendered once
/// per frame via the [`eframe::App`] impl below.
pub struct App {
    /// Show the "About" modal.
    pub show_about: bool,
    /// Background task manager -- owns every running / queued task.
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
    import_summary_rx: Option<std::sync::mpsc::Receiver<ImportSummary>>,

    /// Logo texture for the About dialog.
    logo: Option<egui::TextureHandle>,

    /// Whether to show the first-launch setup dialog.
    pub show_setup_dialog: bool,
    /// Collection name entered in the setup dialog.
    pub setup_name: String,
    /// Directory chosen in the setup dialog.
    pub setup_dir: PathBuf,
    /// Last error from catalog creation in the setup dialog.
    pub setup_error: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        let picture_dir = || -> PathBuf {
            UserDirs::new()
                .and_then(|u| u.picture_dir().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."))
        };

        let (catalog, catalog_counts, catalog_error, show_setup_dialog, setup_name, setup_dir, setup_error) =
            match Catalog::default_path() {
                Ok(p) => match Catalog::open_existing(&p) {
                    Ok(c) => {
                        let counts = c.counts().ok();
                        (Some(Arc::new(c)), counts, None, false, String::new(), PathBuf::new(), None)
                    }
                    Err(e) => {
                        let is_not_found =
                            matches!(&e, crate::catalog::CatalogError::NotFound(_));
                        (
                            None,
                            None,
                            if is_not_found { None } else { Some(e.to_string()) },
                            true,
                            "realraw".to_string(),
                            picture_dir(),
                            None,
                        )
                    }
                },
                Err(e) => (
                    None,
                    None,
                    Some(e.to_string()),
                    true,
                    "realraw".to_string(),
                    picture_dir(),
                    None,
                ),
            };
        let mut library = LibraryPage::default();
        if let Some(cat) = catalog.as_ref() {
            library.refresh(cat, None);
        }
        let library_last_refresh_mtime_ms = catalog
            .as_ref()
            .and_then(|c| std::fs::metadata(c.path()).ok())
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64);
        Self {
            show_about: false,
            task_manager: TaskManager::new().set_max_concurrency(4),
            last_snapshot: TaskSnapshot::default(),
            next_demo_id: 1,
            tasks_open: false,
            all_done_at: None,
            catalog,
            catalog_counts,
            catalog_error,
            import_dialog: None,
            library,
            library_last_refresh_mtime_ms,
            library_needs_refresh: false,
            last_dialog_phase: None,
            import_summary_rx: None,
            logo: None,
            show_setup_dialog,
            setup_name,
            setup_dir,
            setup_error,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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

        render_menubar(self, ctx, show_badge, overall_progress);
        render_tasks_panel(self, ctx, has_running, running, total);
        render_status_bar(self, ctx);
        render_central(self, ctx);

        if self.show_setup_dialog {
            render_setup_dialog(self, ctx);
        }

        if self.show_about {
            render_about_modal(self, ctx);
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

// ---------------------------------------------------------------------------
// View pieces -- small helpers so update() reads top-to-bottom.
// ---------------------------------------------------------------------------

fn render_menubar(
    app: &mut App,
    ctx: &egui::Context,
    show_badge: bool,
    overall_progress: f32,
) {
    egui::TopBottomPanel::top("menubar").show(ctx, |ui| {
        egui::menu::bar(ui, |ui| {
            ui.menu_button("File", |ui| file_menu(ui, app));
            ui.menu_button("Edit", edit_menu);
            ui.menu_button("Library", |ui| library_menu(ui, app));
            ui.menu_button("Photo", photo_menu);
            ui.menu_button("View", view_menu);
            ui.menu_button("Help", |ui| {
                if ui.button("About").clicked() {
                    app.show_about = true;
                    ui.close_menu();
                }
            });

            // Right-aligned badge: "Tasks" toggle + overall progress bar.
            // The badge hides itself 1s after every task completes.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if show_badge {
                    if ui
                        .selectable_label(app.tasks_open, "Tasks")
                        .clicked()
                    {
                        app.tasks_open = !app.tasks_open;
                    }
                    ui.add(
                        egui::ProgressBar::new(overall_progress)
                            .desired_width(110.0)
                            .show_percentage(),
                    );
                }
            });
        });
    });
}

fn render_tasks_panel(
    app: &mut App,
    ctx: &egui::Context,
    has_running: bool,
    running: usize,
    total: usize,
) {
    egui::TopBottomPanel::bottom("background_tasks")
        .resizable(false)
        .show_animated(ctx, app.tasks_open, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Running");
                if has_running {
                    ui.weak(format!("({running})"));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("x").on_hover_text("Close").clicked() {
                        app.tasks_open = false;
                    }
                    if has_running && ui.small_button("Cancel all").clicked() {
                        cancel_all_non_terminal(app);
                    }
                    if total > 0 && !has_running && ui.small_button("Clear").clicked() {
                        app.task_manager = TaskManager::new().set_max_concurrency(4);
                    }
                });
            });
            ui.separator();

            let opts = TaskViewOptions {
                compact: true,
                flat: true,
                only_running: true,
                ..TaskViewOptions::default()
            };
            let mut on_command = |cmd: TaskCommand| match cmd {
                TaskCommand::CancelTask(id) => app.task_manager.cancel(id),
                TaskCommand::CancelGroup(gid) => app.task_manager.cancel_group(gid),
                TaskCommand::ToggleGroup(gid, collapsed) => {
                    app.task_manager.set_group_collapsed(gid, collapsed)
                }
            };
            egui::ScrollArea::vertical()
                .max_height(280.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if has_running {
                        crate::task::task_tree(
                            ui,
                            &app.last_snapshot,
                            &opts,
                            &mut on_command,
                        );
                    } else {
                        ui.weak("Nothing running.");
                    }
                });
        });
}

fn render_central(app: &mut App, ctx: &egui::Context) {
    egui::CentralPanel::default().show(ctx, |ui| {
        // Refresh triggers (any one fires the refresh):
        // 1. The import dialog set `library_needs_refresh` when its
        //    batch finished. This is the fast, accurate path.
        // 2. The catalog file's mtime changed (catches external
        //    edits, e.g. another tool imported photos). We use
        //    millisecond precision so a sub-second import still
        //    registers; some filesystems round to whole seconds.
        let mut needs_refresh = app.library_needs_refresh;
        if !needs_refresh
            && let Some(cat) = &app.catalog
        {
            let mtime_ms = std::fs::metadata(cat.path())
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64);
            if mtime_ms != app.library_last_refresh_mtime_ms {
                app.library_last_refresh_mtime_ms = mtime_ms;
                needs_refresh = true;
            }
        }
        if needs_refresh {
            app.library_needs_refresh = false;
            if let Some(cat) = &app.catalog {
                app.library.refresh(cat, None);
            }
        }
        if let Some(cat) = &app.catalog {
            app.library.importing = app.import_summary_rx.is_some();
            let _ = app.library.show(ctx, ui, cat);
        }
    });
}

fn render_about_modal(app: &mut App, ctx: &egui::Context) {
    let logo = app.logo.get_or_insert_with(|| {
        let img = image::load_from_memory(include_bytes!("../../assets/icon-64.png"))
            .expect("Failed to decode logo");
        let rgba = img.to_rgba8();
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [64, 64],
            rgba.as_raw(),
        );
        ctx.load_texture("logo", color_image, egui::TextureOptions::LINEAR)
    });

    let response = egui::Modal::new(egui::Id::new("about_modal")).show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(8.0);
            ui.image(&*logo);
            ui.add_space(8.0);
        });
        ui.heading("realraw");
        ui.label("An open source Lightroom alternative.");
        ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
        ui.add_space(8.0);
        ui.hyperlink_to("github.com/devsker/realraw", "https://github.com/devsker/realraw");
        ui.add_space(8.0);
        ui.vertical_centered(|ui| {
            if ui.button("Close").clicked() {
                app.show_about = false;
            }
        });
    });
    if response.should_close() {
        app.show_about = false;
    }
}

fn render_setup_dialog(app: &mut App, ctx: &egui::Context) {
    let mut catalog_created = false;

    let response = egui::Modal::new(egui::Id::new("setup_dialog")).show(ctx, |ui| {
        ui.heading("Welcome to realraw");
        ui.label("Create your first collection to get started.");
        ui.add_space(12.0);

        ui.horizontal(|ui| {
            ui.label("Collection name:");
            ui.add(
                egui::TextEdit::singleline(&mut app.setup_name)
                    .hint_text("realraw")
                    .desired_width(240.0),
            );
        });

        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label("Save location:");
            ui.label(app.setup_dir.display().to_string());
            if ui.button("Browse...").clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    app.setup_dir = p;
                }
            }
        });

        ui.add_space(16.0);

        if let Some(ref err) = app.setup_error {
            ui.colored_label(egui::Color32::LIGHT_RED, err);
        }

        ui.horizontal(|ui| {
            let can_create = !app.setup_name.trim().is_empty();
            if ui
                .add_enabled(can_create, egui::Button::new("Create Catalog"))
                .clicked()
            {
                let dir = app.setup_dir.join(app.setup_name.trim());
                let path = dir.join("catalog.sqlite");
                match Catalog::create(&path) {
                    Ok(cat) => {
                        let counts = cat.counts().ok();
                        app.catalog = Some(Arc::new(cat));
                        app.catalog_counts = counts;
                        app.catalog_error = None;
                        app.setup_error = None;
                        catalog_created = true;
                    }
                    Err(e) => {
                        app.setup_error = Some(e.to_string());
                    }
                }
            }
            if ui.button("Cancel").clicked() {
                catalog_created = true;
            }
        });
    });

    if response.should_close() || catalog_created {
        if app.catalog.is_some() {
            if let Some(cat) = app.catalog.as_ref() {
                app.library.refresh(cat, None);
            }
            app.library_last_refresh_mtime_ms = app
                .catalog
                .as_ref()
                .and_then(|c| std::fs::metadata(c.path()).ok())
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64);
        }
        app.setup_error = None;
        app.show_setup_dialog = false;
    }
}

fn cancel_all_non_terminal(app: &mut App) {
    let ids: Vec<_> = app
        .last_snapshot
        .tasks
        .iter()
        .filter(|t| !t.status().is_terminal())
        .map(|t| t.id())
        .collect();
    for id in ids {
        app.task_manager.cancel(id);
    }
}

fn render_status_bar(app: &mut App, ctx: &egui::Context) {
    egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
        ui.horizontal(|ui| {
            if let Some(cat) = &app.catalog {
                // Refresh counts each frame so the status bar reflects inserts.
                if let Ok(counts) = cat.counts() {
                    app.catalog_counts = Some(counts);
                }
                let path = cat.display_path();
                ui.strong("catalog:");
                ui.label(&path);
                if let Some(c) = app.catalog_counts {
                    ui.separator();
                    ui.label(format!("photos: {}", c.photos));
                }
            } else if let Some(err) = &app.catalog_error {
                ui.colored_label(egui::Color32::LIGHT_RED, err);
            } else {
                ui.weak("no catalog open");
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Menus.
// ---------------------------------------------------------------------------

fn file_menu(ui: &mut egui::Ui, app: &mut App) {
    if ui.button("Import Photos...").clicked() {
        app.import_dialog = Some(ImportDialog::default());
        ui.close_menu();
    }
    if ui.button("Open Catalog...").clicked() {
        ui.close_menu();
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("SQLite", &["sqlite", "db"])
            .pick_file()
        {
            try_open_catalog(app, &path);
        }
    }
    if ui.button("New Catalog...").clicked() {
        ui.close_menu();
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("SQLite", &["sqlite", "db"])
            .set_file_name("catalog.sqlite")
            .save_file()
        {
            try_new_catalog(app, &path);
        }
    }
    if ui.button("Open Recent").clicked() { ui.close_menu(); }
    ui.separator();
    if ui.button("Export...").clicked() { ui.close_menu(); }
    ui.separator();
    if ui.button("Quit").clicked() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        ui.close_menu();
    }
}

fn try_open_catalog(app: &mut App, path: &std::path::Path) {
    match Catalog::open(path) {
        Ok(cat) => {
            let counts = cat.counts().ok();
            app.catalog_error = None;
            app.catalog_counts = counts;
            app.catalog = Some(Arc::new(cat));
        }
        Err(e) => {
            app.catalog_error = Some(format!("open failed: {e}"));
        }
    }
}

fn try_new_catalog(app: &mut App, path: &std::path::Path) {
    match Catalog::create(path) {
        Ok(cat) => {
            let counts = cat.counts().ok();
            app.catalog_error = None;
            app.catalog_counts = counts;
            app.catalog = Some(Arc::new(cat));
        }
        Err(e) => {
            app.catalog_error = Some(format!("create failed: {e}"));
        }
    }
}

fn edit_menu(ui: &mut egui::Ui) {
    if ui.button("Undo").clicked() { ui.close_menu(); }
    if ui.button("Redo").clicked() { ui.close_menu(); }
    ui.separator();
    if ui.button("Cut").clicked() { ui.close_menu(); }
    if ui.button("Copy").clicked() { ui.close_menu(); }
    if ui.button("Paste").clicked() { ui.close_menu(); }
}

fn library_menu(ui: &mut egui::Ui, app: &mut App) {
    if ui.button("Import Photos...").clicked() {
        app.import_dialog = Some(ImportDialog::default());
        ui.close_menu();
    }
    if ui.button("New Catalog...").clicked() { ui.close_menu(); }
    ui.separator();
    if ui.button("Find").clicked() { ui.close_menu(); }
    if ui.button("Flag as Picked").clicked() { ui.close_menu(); }
    if ui.button("Flag as Rejected").clicked() { ui.close_menu(); }
    if ui.button("Add Keyword").clicked() { ui.close_menu(); }
    ui.separator();
    if ui.button("Go to Grid View").clicked() { ui.close_menu(); }
    if ui.button("Go to Loupe View").clicked() { ui.close_menu(); }
}

fn photo_menu(ui: &mut egui::Ui) {
    if ui.button("Edit In").clicked() { ui.close_menu(); }
    ui.separator();
    if ui.button("Go to Develop").clicked() { ui.close_menu(); }
    if ui.button("Go to Library").clicked() { ui.close_menu(); }
    ui.separator();
    if ui.button("Create Virtual Copy").clicked() { ui.close_menu(); }
    if ui.button("Go to Next Photo").clicked() { ui.close_menu(); }
    if ui.button("Go to Previous Photo").clicked() { ui.close_menu(); }
}

fn view_menu(ui: &mut egui::Ui) {
    if ui.button("Zoom In").clicked() { ui.close_menu(); }
    if ui.button("Zoom Out").clicked() { ui.close_menu(); }
    if ui.button("Fit on Screen").clicked() { ui.close_menu(); }
    if ui.button("Fill Frame").clicked() { ui.close_menu(); }
    if ui.button("1:1 Pixels").clicked() { ui.close_menu(); }
    ui.separator();
    if ui.button("Loupe").clicked() { ui.close_menu(); }
    if ui.button("Grid").clicked() { ui.close_menu(); }
    if ui.button("Compare").clicked() { ui.close_menu(); }
    if ui.button("Survey").clicked() { ui.close_menu(); }
    ui.separator();
    if ui.button("Fullscreen").clicked() { ui.close_menu(); }
}
