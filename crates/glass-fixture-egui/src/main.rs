//! glass-fixture-egui — a tiny eframe/egui (0.34) app exposing a known accesskit surface for
//! on-box a11y tests. Every interaction logs one line to stdout (the ground-truth oracle).
//! Excluded from the workspace build (heavy egui deps; on-box-test-only).

use std::io::Write;

use eframe::egui;

fn log(line: &str) {
    println!("{line}");
    let _ = std::io::stdout().flush();
}

/// Wrap `add_contents` in an unnamed container exposed to the accessibility tree with an
/// explicit `Pane` role, rather than the plain `Frame::group`/`ui.group` default. A plain
/// group's container is registered internally with accesskit's `GenericContainer` role, which
/// accesskit's own AT-SPI adapter always elides (its node filter drops every
/// `GenericContainer` regardless of name or children — see `accesskit_consumer::filters`), so
/// it would never reach glass's accessibility tree. `Pane` is a distinct, non-generic role
/// that survives that filter, so it lands in the a11y tree as a real, unnamed, single-child
/// container — the wrapper chain this fixture's on-box a11y tests need `render_compact` to
/// have something to collapse.
fn wrap_in_pane<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let mut prepared = egui::Frame::group(ui.style()).begin(ui);
    ui.ctx()
        .accesskit_node_builder(prepared.content_ui.unique_id(), |node| {
            node.set_role(egui::accesskit::Role::Pane);
        });
    let ret = add_contents(&mut prepared.content_ui);
    prepared.end(ui);
    ret
}

#[derive(Default)]
struct Fixture {
    text: String,
    value: f32,
    announced: bool,
    frames: u32,
    copied: bool,
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
        // Write the clipboard once via egui (-> arboard -> user32 SetClipboardData), a few frames in
        // so the host's private-clipboard store/pipe is up. Tests whether a contained app's own
        // clipboard write is readable by glass.
        self.frames += 1;
        if !self.copied && self.frames >= 60 {
            self.copied = true;
            ui.ctx().copy_text("GLASS-CLIP-SENTINEL".to_string());
            log("[fixture] copied sentinel");
        }
        // Report each wheel event with BOTH the event-level modifiers and the frame-aggregate
        // modifiers, so on-box tests can verify wheel + modifier delivery AND that the modifier is
        // held across the wheel's frame (the layer the egui `i.modifiers` handler idiom reads).
        ui.input(|i| {
            for ev in &i.raw.events {
                match ev {
                    // `ev_*` are the modifiers carried ON the wheel event; `frame_*` are the
                    // frame-aggregate `i.modifiers` a handler actually gates on. They diverge when a
                    // synthetic ctrl+wheel is injected as one burst: the event carries ctrl, but the
                    // frame-aggregate reads released because the modifier is pressed and released
                    // within a single frame — so `i.modifiers.ctrl` is false. (ctrl+wheel also routes
                    // to a zoom gesture, zeroing smooth_scroll_delta.)
                    egui::Event::MouseWheel { delta, modifiers, .. } => log(&format!(
                        "[fixture] wheel delta=({:.1},{:.1}) ev_ctrl={} ev_shift={} frame_ctrl={} frame_shift={} smooth_scroll_y={:.2} zoom_delta={:.4}",
                        delta.x, delta.y, modifiers.ctrl, modifiers.shift,
                        i.modifiers.ctrl, i.modifiers.shift,
                        i.smooth_scroll_delta.y, i.zoom_delta()
                    )),
                    // Each key event carries its own (event-level) modifiers.
                    egui::Event::Key { key, pressed, modifiers, .. } => log(&format!(
                        "[fixture] key {key:?} pressed={pressed} ev_ctrl={} ev_cmd={}",
                        modifiers.ctrl, modifiers.command
                    )),
                    _ => {}
                }
            }
            // The standard egui hotkey idiom reads the FRAME-AGGREGATE modifier alongside
            // key_pressed. glass_key "ctrl+z" must let `key_pressed(Z) && modifiers.command` hold in
            // one frame — it can't if glass releases ctrl in the same frame the key arrives.
            if i.key_pressed(egui::Key::Z) {
                log(&format!(
                    "[fixture] chord Z: frame_ctrl={} frame_cmd={} undo_idiom={}",
                    i.modifiers.ctrl, i.modifiers.command, i.modifiers.command
                ));
            }
        });
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
            // Nest Apply inside a pair of unnamed, single-child panes (see `wrap_in_pane`) —
            // this fixture's accessibility tree is otherwise flat, so on-box a11y tests that
            // assert the outline's compact render is smaller than its full render need this
            // chain to have something to collapse. Doesn't change Apply's own role, name, or
            // behavior.
            wrap_in_pane(ui, |ui| {
                wrap_in_pane(ui, |ui| {
                    if ui.button("Apply").clicked() {
                        log("[fixture] apply");
                    }
                });
            });
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
