#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod demo;
#[allow(dead_code)] // Wired up with the working interface.
mod easter;
mod skin;
mod theme;
mod widgets;

use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;

/// Command-line switches, kept to what screenshots and CI need.
pub struct Launch {
    /// Show the fixed demo catalog instead of touching the network or game folder.
    pub demo: bool,
    /// Render a few frames, save a PNG here, and exit.
    pub screenshot: Option<PathBuf>,
}

fn window_icon() -> Arc<egui::IconData> {
    let img = image::load_from_memory_with_format(
        include_bytes!("../../../assets/icon/png/icon-64.png"),
        image::ImageFormat::Png,
    )
    .expect("the embedded icon is a valid PNG")
    .into_rgba8();
    Arc::new(egui::IconData {
        width: img.width(),
        height: img.height(),
        rgba: img.into_raw(),
    })
}

fn main() -> eframe::Result {
    let mut launch = Launch {
        demo: false,
        screenshot: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--demo" => launch.demo = true,
            "--screenshot" => launch.screenshot = args.next().map(PathBuf::from),
            _ => {}
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("WoW is a Feeling")
            .with_app_id("wow-is-a-feeling")
            .with_inner_size([1040.0, 700.0])
            .with_min_inner_size([820.0, 560.0])
            .with_decorations(false)
            .with_icon(window_icon()),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "wow-is-a-feeling",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, launch)))),
    )
}
