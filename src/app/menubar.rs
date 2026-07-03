use eframe::egui;

use crate::app::App;
use crate::catalog::Catalog;
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
        app.toasts.add("Unimplemented");
        ui.close_menu();
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

pub(crate) fn try_new_catalog(app: &mut App, path: &std::path::Path) {
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
        app.toasts.add("Unimplemented");
        ui.close_menu();
    }
    if ui.button("Go to Library").clicked() {
        app.toasts.add("Unimplemented");
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
