use eframe::egui::{self, ViewportBuilder};
use eframe::Renderer;
use realraw::app::App;

fn main() -> eframe::Result<()> {
    let icon = eframe::icon_data::from_png_bytes(realraw::ICON_PNG)
        .expect("Failed to decode app icon");
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default().with_icon(icon),
        renderer: Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "realraw",
        options,
        Box::new(|cc| {
            let mut fonts = egui::FontDefinitions::default();
            egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
            cc.egui_ctx.set_fonts(fonts);
            Ok(Box::new(App::default()))
        }),
    )
}
