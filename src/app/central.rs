use eframe::egui;

use crate::app::App;
use crate::app::app::Page;

pub(crate) fn render(app: &mut App, ctx: &egui::Context) {
    match app.current_page {
        Page::Library => {
            egui::CentralPanel::default().show(ctx, |ui| {
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
                    let _ = app.library.show(ctx, ui, cat.clone(), &mut app.task_manager);

                    // Navigate to Develop mode if a photo was double-clicked.
                    if let Some(id) = app.library.activated_id.take() {
                        app.develop_photo_id = Some(id);
                        app.current_page = Page::Develop;
                    }
                }
            });
        }
        Page::Develop => {
            crate::app::develop::render(app, ctx);
        }
    }
}
