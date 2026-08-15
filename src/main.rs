// Kite — a video editor built to stay responsive on modest hardware.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod audio;
mod decode;
mod export;
mod ffmpeg;
mod framestore;
mod import;
mod project;
mod selftest;
mod theme;
mod timeline;
mod ui_chrome;

use std::sync::Arc;

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let tools = match ffmpeg::Tools::discover() {
        Ok(t) => Arc::new(t),
        Err(e) => {
            let msg = format!(
                "Kite could not find ffmpeg.\n\n{e}\n\n\
                 If you installed Kite with the installer this should not happen — \
                 please reinstall. If you are running from a build folder, put ffmpeg.exe and \
                 ffprobe.exe in an 'ffmpeg' folder next to kite.exe."
            );
            rfd::MessageDialog::new()
                .set_title("Kite")
                .set_description(msg)
                .set_level(rfd::MessageLevel::Error)
                .show();
            std::process::exit(1);
        }
    };

    if args.iter().any(|a| a == "--selftest") {
        return match selftest::run(tools) {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("\nSELF-TEST FAILED: {e:#}");
                std::process::exit(1);
            }
        };
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 880.0])
            .with_min_inner_size([980.0, 620.0])
            .with_title("Kite")
            .with_drag_and_drop(true),
        // vsync keeps us at display rate without spinning the GPU on an idle timeline.
        vsync: true,
        ..Default::default()
    };

    let open_path = args
        .iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("kite")));

    let result = eframe::run_native(
        "Kite",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, tools, open_path)))),
    );

    // A GUI-subsystem binary has nowhere to print, so a failure to open the window would
    // otherwise look like the application simply not starting.
    if let Err(e) = &result {
        rfd::MessageDialog::new()
            .set_title("Kite could not start")
            .set_description(format!(
                "{e}\n\nThis usually means the graphics driver could not provide a Direct3D 12 \
                 or Vulkan device. Updating your graphics driver normally fixes it."
            ))
            .set_level(rfd::MessageLevel::Error)
            .show();
    }
    result
}
