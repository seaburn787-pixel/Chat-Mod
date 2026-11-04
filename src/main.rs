// No console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod filter;
mod model;
mod moderation;
mod sound;
mod source;
mod spelled;
mod yt_mod;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Chat Mod")
            .with_inner_size([1200.0, 750.0])
            .with_min_inner_size([700.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Chat Mod",
        options,
        Box::new(|cc| Ok(Box::new(app::ModApp::new(cc)))),
    )
}
