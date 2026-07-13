use std::sync::Arc;

use eframe::egui;

use crate::app::App;
use crate::catalog::Catalog;

pub(crate) fn render(app: &mut App, ctx: &egui::Context) {
    let mut catalog_created = false;

    egui::Modal::new(egui::Id::new("setup_dialog")).show(ctx, |ui| {
        ui.heading("Welcome to realraw");
        ui.label("Create your first collection or open an existing one.");
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
            if ui.button("Browse...").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                app.setup_dir = p;
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
                        Catalog::save_last_path(cat.path());
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
        });

        ui.add_space(4.0);

        if ui.button("Open Existing Collection...").clicked()
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("SQLite", &["sqlite", "db"])
                .pick_file()
        {
            match Catalog::open(&path) {
                Ok(cat) => {
                    Catalog::save_last_path(cat.path());
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
    });

    if catalog_created {
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
        app.setup_error = None;
        app.show_setup_dialog = false;
    }
}
