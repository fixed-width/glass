//! glass-fixture-egui — a tiny eframe/egui (0.34) app exposing a known accesskit surface for
//! on-box a11y tests. Every interaction logs one line to stdout (the ground-truth oracle).
//! Excluded from the workspace build (heavy egui deps; on-box-test-only).

use std::io::Write;

use eframe::egui;

fn log(line: &str) {
    println!("{line}");
    let _ = std::io::stdout().flush();
}

#[derive(Default)]
struct Fixture {
    text: String,
    value: f32,
    announced: bool,
}

impl eframe::App for Fixture {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Keep the event loop hot so the accesskit UIA provider stays responsive — a fully idle
        // egui app can leave the provider not answering UIA queries (a11y snapshot times out).
        ui.ctx().request_repaint();
        if !self.announced {
            log("[fixture] ready");
            self.announced = true;
        }
        egui::CentralPanel::default().show_inside(ui, |ui| {
            let label = ui.label("Text:");
            if ui
                .text_edit_singleline(&mut self.text)
                .labelled_by(label.id)
                .changed()
            {
                log(&format!("[fixture] text={}", self.text));
            }
            if ui
                .add(egui::Slider::new(&mut self.value, 0.0..=100.0).text("Value"))
                .changed()
            {
                log(&format!("[fixture] value={}", self.value));
            }
            if ui.button("Apply").clicked() {
                log("[fixture] apply");
            }
        });
    }
}

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([400.0, 300.0])
            .with_title("glass-fixture-egui"),
        ..Default::default()
    };
    eframe::run_native(
        "glass-fixture-egui",
        native_options,
        Box::new(|_cc| Ok(Box::new(Fixture::default()))),
    )
}
