//! Native form and value producer for external interaction benchmarks.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eframe::egui;

#[derive(Default)]
struct Interaction {
    source: bool,
    account: String,
    saved: String,
    submissions: u32,
    generated: String,
    generations: u32,
}

fn readout(ui: &mut egui::Ui, name: &str, value: &str) {
    let label = ui.label(name);
    let mut text = value.to_owned();
    ui.add(egui::TextEdit::singleline(&mut text).interactive(false))
        .labelled_by(label.id);
}

impl eframe::App for Interaction {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint_after(Duration::from_millis(50));
        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.heading("Interaction native");
            readout(ui, "Fixture ready", "ready");
            if self.source {
                readout(ui, "Source value", &self.generated);
                readout(ui, "Generation count", &self.generations.to_string());
                if ui.button("Generate transfer").clicked() {
                    self.generations += 1;
                    let timestamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos();
                    self.generated = format!("ticket-{}-{timestamp:032x}", std::process::id());
                }
            } else {
                let label = ui.label("Account name");
                ui.text_edit_singleline(&mut self.account)
                    .labelled_by(label.id);
                if ui.button("Save account").clicked() {
                    self.saved.clone_from(&self.account);
                    self.submissions += 1;
                }
                readout(ui, "Saved value", &self.saved);
                readout(ui, "Submission count", &self.submissions.to_string());
            }
        });
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let source = match args.as_slice() {
        [] => false,
        [flag] if flag == "--source" => true,
        _ => return Err("usage: glass-interaction-native [--source]".into()),
    };
    eframe::run_native(
        "Interaction native",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([600.0, 500.0])
                .with_title("Interaction native"),
            ..Default::default()
        },
        Box::new(move |_cc| {
            Ok(Box::new(Interaction {
                source,
                ..Default::default()
            }))
        }),
    )?;
    Ok(())
}
