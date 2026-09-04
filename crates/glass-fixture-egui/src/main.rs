//! glass-fixture-egui — a tiny eframe/egui (0.34) app exposing a known accesskit surface for
//! on-box a11y tests. Every interaction logs one line to stdout (the ground-truth oracle).
//! Excluded from the workspace build (heavy egui deps; on-box-test-only).

use std::io::Write;
use std::time::{Duration, Instant};

use eframe::egui;

const DEFAULT_MOVEMENT_DURATION: Duration = Duration::from_millis(300);

fn parse_movement_duration(value: Option<&str>) -> Result<Duration, &'static str> {
    let Some(value) = value else {
        return Ok(DEFAULT_MOVEMENT_DURATION);
    };
    let invalid = "--movement-duration-ms must be an integer from 1 to 5000";
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid);
    }
    let millis = value.parse::<u64>().map_err(|_| invalid)?;
    if !(1..=5000).contains(&millis) {
        return Err(invalid);
    }
    Ok(Duration::from_millis(millis))
}

fn movement_duration_from_args<'a>(
    args: impl IntoIterator<Item = &'a str>,
) -> Result<Duration, &'static str> {
    let mut args = args.into_iter();
    let Some(arg) = args.next() else {
        return parse_movement_duration(None);
    };
    let value = arg
        .strip_prefix("--movement-duration-ms=")
        .ok_or("expected --movement-duration-ms=<milliseconds>")?;
    if args.next().is_some() {
        return Err("only one movement duration option is allowed");
    }
    parse_movement_duration(Some(value))
}

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

fn wrap_in_semantic_button<R>(
    ui: &mut egui::Ui,
    label: &str,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let mut prepared = egui::Frame::group(ui.style()).begin(ui);
    let id = prepared.content_ui.unique_id();
    ui.ctx().accesskit_node_builder(id, |node| {
        node.set_role(egui::accesskit::Role::Button);
        node.set_label(label);
    });
    let ret = add_contents(&mut prepared.content_ui);
    let rect = prepared.end(ui).rect;
    ui.ctx().accesskit_node_builder(id, |node| {
        node.set_bounds(egui::accesskit::Rect {
            x0: f64::from(rect.left()),
            y0: f64::from(rect.top()),
            x1: f64::from(rect.right()),
            y1: f64::from(rect.bottom()),
        });
    });
    ret
}

#[derive(Default)]
struct Fixture {
    text: String,
    value: f32,
    announced: bool,
    frames: u32,
    copied: bool,
    movement_started: Option<Instant>,
    movement_duration: Option<Duration>,
    logs: Vec<&'static str>,
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
            if ui.button("Semantic Save").clicked() {
                self.movement_started = Some(Instant::now());
                self.logs.push("[fixture] semantic_save");
            }
            if ui.button("Restart movement").clicked() {
                self.movement_started = Some(Instant::now());
                self.logs.push("[fixture] movement_restart");
            }
            if ui
                .add_enabled(false, egui::Button::new("Disabled semantic"))
                .clicked()
            {
                self.logs.push("[fixture] disabled_semantic");
            }
            ui.horizontal(|ui| {
                for _ in 0..2 {
                    if ui.button("Duplicate semantic").clicked() {
                        self.logs.push("[fixture] duplicate_semantic");
                    }
                }
            });

            let movement = self.movement_started.map_or(0.0, |started| {
                let duration = self.movement_duration.unwrap_or(DEFAULT_MOVEMENT_DURATION);
                let elapsed = started.elapsed().min(duration);
                if elapsed < duration {
                    ui.ctx().request_repaint();
                }
                450.0 * elapsed.as_secs_f32() / duration.as_secs_f32()
            });
            ui.horizontal(|ui| {
                ui.add_space(movement);
                if ui.button("Moving semantic").clicked() {
                    self.logs.push("[fixture] moving_semantic");
                }
            });

            let occluded = ui.add_sized([180.0, 32.0], egui::Button::new("Occluded semantic"));
            if occluded.clicked() {
                self.logs.push("[fixture] occluded_semantic");
            }
            egui::Area::new(egui::Id::new("semantic-occluder"))
                .order(egui::Order::Foreground)
                .fixed_pos(occluded.rect.min)
                .show(ui.ctx(), |ui| {
                    if ui
                        .add_sized(occluded.rect.size(), egui::Button::new("Occluder"))
                        .clicked()
                    {
                        self.logs.push("[fixture] occluder");
                    }
                });
            wrap_in_semantic_button(ui, "Composite semantic", |ui| {
                if ui.button("Nested semantic").clicked() {
                    self.logs.push("[fixture] nested_semantic");
                }
            });
            let hidden = ui.place(
                egui::Rect::from_min_size(
                    egui::pos2(20.0, ui.ctx().content_rect().bottom() + 150.0),
                    egui::vec2(160.0, 32.0),
                ),
                egui::Button::new("Hidden semantic"),
            );
            if hidden.clicked() {
                self.logs.push("[fixture] hidden_semantic");
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
        for line in self.logs.drain(..) {
            log(line);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args_os()
        .skip(1)
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "fixture arguments must be UTF-8")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let movement_duration = movement_duration_from_args(args.iter().map(String::as_str))?;
    log(&format!(
        "[fixture] movement_duration_ms={}",
        movement_duration.as_millis()
    ));
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([700.0, 500.0])
            .with_title("glass-fixture-egui"),
        ..Default::default()
    };
    eframe::run_native(
        "glass-fixture-egui",
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(Fixture {
                movement_duration: Some(movement_duration),
                ..Default::default()
            }))
        }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movement_arguments_default_or_select_an_explicit_duration() {
        assert_eq!(
            movement_duration_from_args([]).unwrap(),
            Duration::from_millis(300)
        );
        assert_eq!(
            movement_duration_from_args(["--movement-duration-ms=3000"]).unwrap(),
            Duration::from_millis(3000)
        );
    }

    #[test]
    fn movement_arguments_reject_unknown_duplicate_or_malformed_options() {
        for args in [
            vec!["--unknown=3000"],
            vec!["--movement-duration-ms"],
            vec!["--movement-duration-ms="],
            vec!["--movement-duration-ms=no"],
            vec!["--movement-duration-ms=3000", "--movement-duration-ms=3000"],
        ] {
            assert!(movement_duration_from_args(args).is_err());
        }
    }

    #[test]
    fn movement_duration_defaults_to_three_hundred_milliseconds() {
        assert_eq!(
            parse_movement_duration(None).unwrap(),
            Duration::from_millis(300)
        );
    }

    #[test]
    fn movement_duration_accepts_bounded_explicit_milliseconds() {
        for millis in [1, 300, 3000, 5000] {
            assert_eq!(
                parse_movement_duration(Some(&millis.to_string())).unwrap(),
                Duration::from_millis(millis)
            );
        }
    }

    #[test]
    fn movement_duration_rejects_malformed_zero_and_excessive_values() {
        for value in [
            "",
            "0",
            "5001",
            "-1",
            "+3000",
            " 3000",
            "3000 ",
            "3s",
            "3.0",
            "999999999999999999999999999",
        ] {
            assert!(
                parse_movement_duration(Some(value)).is_err(),
                "must reject {value:?}"
            );
        }
    }

    #[test]
    fn movement_duration_override_moves_longer_without_leaving_the_viewport() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut fixture = Fixture {
            movement_duration: Some(parse_movement_duration(Some("3000")).unwrap()),
            ..Default::default()
        };
        let mut frame = eframe::Frame::_new_kittest();
        let mut bounds_at = |elapsed| {
            fixture.movement_started = Some(Instant::now() - elapsed);
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(700.0, 500.0),
                    )),
                    ..Default::default()
                },
                |ui| eframe::App::ui(&mut fixture, ui, &mut frame),
            );
            output
                .platform_output
                .accesskit_update
                .unwrap()
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some("Moving semantic"))
                .unwrap()
                .1
                .bounds()
                .unwrap()
        };
        let early = bounds_at(Duration::from_millis(350));
        let later = bounds_at(Duration::from_millis(1000));
        let final_bounds = bounds_at(Duration::from_millis(3100));
        assert!(early.x0 < later.x0 && later.x0 < final_bounds.x0);
        assert!(final_bounds.x1 <= 700.0);
        assert_eq!(final_bounds, bounds_at(Duration::from_millis(4000)));
    }

    #[test]
    fn moving_semantic_holds_fixed_after_three_hundred_milliseconds() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut fixture = Fixture::default();
        let mut frame = eframe::Frame::_new_kittest();
        let mut bounds_at = |elapsed| {
            fixture.movement_started = Some(Instant::now() - elapsed);
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(700.0, 500.0),
                    )),
                    ..Default::default()
                },
                |ui| eframe::App::ui(&mut fixture, ui, &mut frame),
            );
            output
                .platform_output
                .accesskit_update
                .expect("AccessKit update")
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some("Moving semantic"))
                .expect("moving control")
                .1
                .bounds()
                .expect("moving bounds")
        };
        assert_eq!(
            bounds_at(Duration::from_millis(350)),
            bounds_at(Duration::from_secs(1))
        );
    }

    #[test]
    fn hidden_semantic_is_physically_outside_the_viewport() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut fixture = Fixture::default();
        let mut frame = eframe::Frame::_new_kittest();
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(700.0, 500.0));
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(viewport),
                ..Default::default()
            },
            |ui| eframe::App::ui(&mut fixture, ui, &mut frame),
        );
        let update = output
            .platform_output
            .accesskit_update
            .expect("AccessKit update");
        let (id, node) = update
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Hidden semantic"))
            .expect("hidden control remains discoverable");
        // SAFETY: this ID came directly from egui's AccessKit update, which preserves Id bits.
        let widget_id = unsafe { egui::Id::from_high_entropy_bits(id.0) };
        let response = ctx.read_response(widget_id).expect("real widget response");
        assert!(
            !viewport.intersects(response.rect),
            "hidden widget is actually drawn at {:?}",
            response.rect
        );
        let bounds = node.bounds().expect("hidden control has bounds");
        assert_eq!(bounds.y0, f64::from(response.rect.top()));
    }

    #[test]
    fn composite_accesskit_bounds_enclose_the_nested_button_center() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut nested = egui::Rect::NOTHING;
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            wrap_in_semantic_button(ui, "Composite semantic", |ui| {
                nested = ui.button("Nested semantic").rect;
            });
        });
        let update = output
            .platform_output
            .accesskit_update
            .expect("AccessKit update");
        let composite = update
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Composite semantic"))
            .expect("composite node");
        let bounds = composite
            .1
            .bounds()
            .expect("composite exposes actual hit geometry");
        let center = nested.center();
        assert!(bounds.x0 <= f64::from(center.x) && f64::from(center.x) < bounds.x1);
        assert!(bounds.y0 <= f64::from(center.y) && f64::from(center.y) < bounds.y1);
    }
}
