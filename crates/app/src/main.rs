#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("WoW is a Feeling")
            .with_inner_size([960.0, 640.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_ui_native("WoW is a Feeling", options, |ui, _frame| {
        ui.heading("WoW is a Feeling");
        ui.label("Baseline window, nothing here yet.");
    })
}
