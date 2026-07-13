use eframe::egui;

use crate::app::App;

pub(crate) fn render(app: &mut App, ctx: &egui::Context) {
    let response = egui::Modal::new(egui::Id::new("about_modal")).show(ctx, |ui| {
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

        ui.vertical_centered(|ui| {
            ui.add_space(8.0);
            ui.image(&*logo);
            ui.add_space(8.0);
            ui.heading("realraw");
            ui.label("An open source Lightroom alternative.");
            ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
            ui.add_space(8.0);
            ui.hyperlink_to("github.com/devsker/realraw", "https://github.com/devsker/realraw");
            ui.hyperlink_to("codeberg.org/sker/realraw", "https://codeberg.org/devsker/realraw");
            ui.add_space(8.0);
            if let Some(gpu) = &app.gpu {
                ui.label(format!("GPU tone: {}", gpu.adapter_label()));
            } else {
                ui.label("GPU tone: not available (using CPU)");
            }
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                if ui.button("Close").clicked() {
                    app.show_about = false;
                }
            });
        });
    });
    if response.should_close() {
        app.show_about = false;
    }
}
