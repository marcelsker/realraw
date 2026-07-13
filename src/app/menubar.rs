use std::path::{Path, PathBuf};

use eframe::egui;

use crate::app::app::Page;
use crate::app::App;
use crate::catalog::Catalog;
use crate::export;
use crate::import::ImportDialog;

pub(crate) fn render(app: &mut App, ctx: &egui::Context, show_badge: bool, overall_progress: f32) {
    egui::TopBottomPanel::top("menubar").show(ctx, |ui| {
        egui::menu::bar(ui, |ui| {
            ui.menu_button("File", |ui| file_menu(ui, app));
            ui.menu_button("Edit", |ui| edit_menu(ui, app));
            ui.menu_button("Library", |ui| library_menu(ui, app));
            ui.menu_button("Photo", |ui| photo_menu(ui, app));
            ui.menu_button("View", |ui| view_menu(ui, app));
            ui.menu_button("Help", |ui| {
                if ui.button("About").clicked() {
                    app.show_about = true;
                    ui.close_menu();
                }
            });

            // Right side: modules (Library / Develop), then tasks badge.
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
                            .desired_width(100.0)
                            .show_percentage(),
                    );
                    ui.add_space(12.0);
                }

                // right_to_left: add Develop first so order reads Library · Develop.
                module_label(ui, app, Page::Develop, "Develop");
                ui.add_space(16.0);
                module_label(ui, app, Page::Library, "Library");
            });
        });
    });
}

/// Flat text module tab (no button chrome). Active = bright + underline.
fn module_label(ui: &mut egui::Ui, app: &mut App, page: Page, label: &str) {
    let selected = app.current_page == page;
    let dark = ui.visuals().dark_mode;

    // Size from a neutral galley, then paint with hover-aware color.
    let font = egui::FontId::proportional(13.5);
    let galley = ui.fonts(|f| {
        f.layout_no_wrap(label.to_owned(), font.clone(), egui::Color32::WHITE)
    });
    let size = galley.size() + egui::vec2(4.0, 6.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());

    let color = if selected {
        if dark {
            egui::Color32::from_rgb(235, 235, 235)
        } else {
            ui.visuals().strong_text_color()
        }
    } else if response.hovered() {
        if dark {
            egui::Color32::from_rgb(190, 190, 190)
        } else {
            ui.visuals().text_color()
        }
    } else if dark {
        egui::Color32::from_rgb(130, 130, 130)
    } else {
        ui.visuals().weak_text_color()
    };

    let galley = ui.fonts(|f| f.layout_no_wrap(label.to_owned(), font, color));
    let text_pos = egui::pos2(
        rect.center().x - galley.size().x * 0.5,
        rect.center().y - galley.size().y * 0.5,
    );
    ui.painter().galley(text_pos, galley, color);

    if selected {
        let y = rect.bottom() - 2.0;
        let inset = 2.0;
        let accent = if dark {
            egui::Color32::from_rgb(210, 210, 210)
        } else {
            ui.visuals().strong_text_color()
        };
        ui.painter().line_segment(
            [
                egui::pos2(rect.left() + inset, y),
                egui::pos2(rect.right() - inset, y),
            ],
            egui::Stroke::new(1.5, accent),
        );
    }

    if response.clicked() {
        app.current_page = page;
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
}

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
    ui.separator();
    if ui.button("Export...").clicked() {
        ui.close_menu();
        export_current_photo(app);
    }
    ui.separator();
    if ui.button("Quit").clicked() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        ui.close_menu();
    }
}

pub(crate) fn try_open_catalog(app: &mut App, path: &std::path::Path) {
    match Catalog::open(path) {
        Ok(cat) => {
            Catalog::save_last_path(cat.path());
            let counts = cat.counts().ok();
            app.catalog_error = None;
            app.catalog_counts = counts;
            app.catalog = Some(std::sync::Arc::new(cat));
        }
        Err(e) => {
            app.catalog_error = Some(format!("open failed: {e}"));
        }
    }
}

pub(crate) fn try_new_catalog(app: &mut App, path: &Path) {
    match Catalog::create(path) {
        Ok(cat) => {
            Catalog::save_last_path(cat.path());
            let counts = cat.counts().ok();
            app.catalog_error = None;
            app.catalog_counts = counts;
            app.catalog = Some(std::sync::Arc::new(cat));
        }
        Err(e) => {
            app.catalog_error = Some(format!("create failed: {e}"));
        }
    }
}

/// File → Export… : develop current photo with current tone, save JPEG/PNG.
fn export_current_photo(app: &mut App) {
    let Some(id) = app.develop_photo_id else {
        app.toasts
            .add("Open a photo in Develop first (double-click in Library).");
        return;
    };
    let Some(cat) = app.catalog.clone() else {
        app.toasts.add("No catalog open");
        return;
    };
    let photo = match cat.find_photo_by_id(id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            app.toasts.add("Photo not found in catalog");
            return;
        }
        Err(e) => {
            app.toasts.add(format!("Catalog error: {e}"));
            return;
        }
    };

    let default_name = Path::new(&photo.path)
        .file_stem()
        .map(|s| format!("{}.jpg", s.to_string_lossy()))
        .unwrap_or_else(|| "export.jpg".into());

    let Some(dest) = rfd::FileDialog::new()
        .add_filter("JPEG", &["jpg", "jpeg"])
        .add_filter("PNG", &["png"])
        .set_file_name(&default_name)
        .save_file()
    else {
        return;
    };
    let dest = export::ensure_export_extension(dest);

    export::spawn_export_task(
        &mut app.task_manager,
        PathBuf::from(&photo.path),
        photo.orientation,
        app.develop.tone(),
        dest,
        app.gpu.clone(),
    );
    app.toasts.add("Export started");
}

fn edit_menu(ui: &mut egui::Ui, app: &mut App) {
    if ui.button("Undo").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("Redo").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    ui.separator();
    if ui.button("Cut").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("Copy").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("Paste").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
}

fn library_menu(ui: &mut egui::Ui, app: &mut App) {
    if ui.button("Import Photos...").clicked() {
        app.import_dialog = Some(ImportDialog::default());
        ui.close_menu();
    }
    if ui.button("New Catalog...").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    ui.separator();
    if ui.button("Find").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("Flag as Picked").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("Flag as Rejected").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("Add Keyword").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
}

fn photo_menu(ui: &mut egui::Ui, app: &mut App) {
    if ui.button("Edit In").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    ui.separator();
    if ui.button("Go to Develop").clicked() {
        app.current_page = Page::Develop;
        ui.close_menu();
    }
    if ui.button("Go to Library").clicked() {
        app.current_page = Page::Library;
        ui.close_menu();
    }
    ui.separator();
    if ui.button("Create Virtual Copy").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("Go to Next Photo").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("Go to Previous Photo").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
}

fn view_menu(ui: &mut egui::Ui, app: &mut App) {
    if ui.button("Zoom In").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("Zoom Out").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("Fit on Screen").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("Fill Frame").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("1:1 Pixels").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    ui.separator();
    if ui.button("Loupe").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("Grid").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("Compare").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("Survey").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    ui.separator();
    if ui.button("Fullscreen").clicked() {
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
}
