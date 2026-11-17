//! The component gallery: every token and component of `ui::theme`, and every state of the bar's macro
//! widget, on one screen, without a PS5 (`psconnector-view --gallery`). The snapshot test below renders the
//! same screen to `design/screens/` so a change in the look shows up as an image diff.

use std::time::Duration;

use eframe::egui;

use crate::macros::{After, MacroDef, Plan, Runner, hms};
use crate::ui::theme::{self, Tone, icon};
use crate::ui::viewer::bar;

/// Example data for the components; nothing here is real.
pub(crate) struct Gallery {
    runner: Runner,
    address: String,
    recording_name: String,
    codec: u8,
    after: After,
    picture_open: bool,
    slider: f32,
}

impl Gallery {
    pub(crate) fn new() -> Self {
        let example = |name: &str, recording: bool| MacroDef {
            name: name.into(),
            cmd: "python3 -u main.py".into(),
            cwd: None,
            env: Vec::new(),
            recording: recording.then(|| "recording.json".into()),
            plan: Plan { limit_min: 30, after: After::Nothing, repeat: 0 },
        };
        let runner =
            Runner::new(Ok(vec![example("Example recording", true), example("Example macro", false)]), Vec::new(), 0);
        Self {
            runner,
            address: "192.0.2.10".into(),
            recording_name: "Recording 11-17 20:15".into(),
            codec: 0,
            after: After::Nothing,
            picture_open: true,
            slider: 0.0,
        }
    }

    pub(crate) fn ui(&mut self, ui: &mut egui::Ui) {
        let t = theme::tokens(ui);
        let bar = egui::Frame::new().fill(t.surface).inner_margin(egui::Margin::symmetric(8, 6));
        egui::Panel::top("gallery_bar").frame(bar).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.set_min_height(theme::size::CONTROL);
                ui.label(egui::RichText::new("Component gallery").strong().size(theme::size::TITLE));
                theme::vsep(ui);
                let mut pref = ui.ctx().options(|o| o.theme_preference);
                let before = pref;
                theme::segmented(
                    ui,
                    &mut pref,
                    &[
                        (egui::ThemePreference::System, "System"),
                        (egui::ThemePreference::Light, "Light"),
                        (egui::ThemePreference::Dark, "Dark"),
                    ],
                );
                if pref != before {
                    ui.ctx().set_theme(pref);
                }
                ui.label(theme::caption(ui, "design/tokens.json · ui/theme.rs"));
            });
        });
        if self.picture_open {
            let closed = theme::side_panel(
                ui,
                "gallery_side_panel",
                320.0,
                "Side panel",
                Some("Fixed title; the content scrolls with its bar in the panel's own gutter."),
                |ui| {
                    theme::section_label(ui, "Adjust");
                    for name in ["Brightness", "Shadows", "Highlights", "Contrast", "Saturation", "Warmth"] {
                        ui.horizontal(|ui| {
                            ui.label(name);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(theme::caption(ui, format!("{:+.2}", self.slider)));
                            });
                        });
                        ui.spacing_mut().slider_width = ui.available_width();
                        ui.add(egui::Slider::new(&mut self.slider, -1.0..=1.0).show_value(false));
                    }
                },
            );
            if closed {
                self.picture_open = false;
            }
        }
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 10.0;
                self.colours(ui);
                section(ui, "Type", |ui| {
                    ui.label(egui::RichText::new("Heading 20").size(theme::size::HEADING).strong());
                    ui.label(egui::RichText::new("Title 15").size(theme::size::TITLE).strong());
                    ui.label("Body 13: almost all text, labels, buttons, menus");
                    ui.label(theme::caption(ui, "Caption 12: secondary lines in muted"));
                    theme::section_label(ui, "Label 11: section labels");
                    ui.label(
                        egui::RichText::new(format!(
                            "{} {} {} {} {} {} {} {}",
                            icon::PLAY,
                            icon::STOP,
                            icon::SLIDERS,
                            icon::MORE,
                            icon::SPEAKER,
                            icon::PICTURE,
                            icon::RECORD,
                            icon::FLAG
                        ))
                        .size(theme::size::ICON),
                    );
                });
                section(ui, "Buttons", |ui| {
                    ui.horizontal(|ui| {
                        theme::primary(ui, true, "Connect");
                        theme::secondary(ui, true, "Search again");
                        theme::ghost(ui, true, "Cancel");
                        theme::destructive(ui, true, "Delete");
                        theme::primary(ui, false, "Disabled");
                    });
                    ui.horizontal(|ui| {
                        theme::icon_button(ui, icon::SPEAKER, "Mute");
                        theme::icon_button(ui, icon::MORE, "More");
                        theme::icon_button(ui, icon::SLIDERS, "Options");
                        theme::vsep(ui);
                        theme::toggle_button(ui, icon::PICTURE, "Picture", false);
                        if theme::toggle_button(ui, icon::PICTURE, "Picture", self.picture_open).clicked() {
                            self.picture_open = !self.picture_open;
                        }
                    });
                });
                section(ui, "Inputs", |ui| {
                    ui.horizontal(|ui| {
                        theme::segmented(ui, &mut self.codec, &[(0, "H.264"), (1, "H.265"), (2, "HDR")]);
                        theme::segmented(
                            ui,
                            &mut self.after,
                            &[(After::Nothing, "Nothing"), (After::Standby, "Rest mode")],
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.add(theme::field(&mut self.address, theme::size::FORM).desired_width(200.0));
                        theme::primary(ui, true, "Connect");
                    });
                });
                section(ui, "Status", |ui| {
                    ui.horizontal(|ui| {
                        theme::badge(ui, "On", Tone::Success);
                        theme::badge(ui, "Rest", Tone::Neutral);
                    });
                    theme::banner(
                        ui,
                        Some("Cannot reach the PS5"),
                        "Network is unreachable: a VPN may be routing the PS5's network. Turn it off and try again.",
                    );
                });
                section(ui, "Card", |ui| {
                    theme::card(ui, "PS5", Some("A group on the start screen"), |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Living room");
                            theme::badge(ui, "On", Tone::Success);
                        });
                    });
                });
                section(ui, "Macro widget", |ui| {
                    let state = |ui: &mut egui::Ui, name: &str| {
                        ui.label(theme::caption(ui, name));
                    };
                    state(ui, "Idle: combo with Run, options, summary, Record");
                    ui.horizontal(|ui| {
                        bar::macro_widget(ui, &mut self.runner, true);
                    });
                    state(ui, "Running");
                    ui.horizontal(|ui| {
                        bar::running_strip(ui, "Example macro", "00:01:28 · 28:32 left", "hand 7m 8m 9m … → discard N");
                    });
                    state(ui, "Scheduled");
                    ui.horizontal(|ui| {
                        bar::scheduled_strip(ui, "Example macro · at 23:00", "in 01:12:00");
                    });
                    state(ui, "Recording");
                    ui.horizontal(|ui| {
                        bar::recording_strip(
                            ui,
                            &format!("{} · 2 checkpoints · 34 inputs", hms(Duration::from_secs(12))),
                        );
                    });
                    state(ui, "Saving a recording");
                    ui.horizontal(|ui| {
                        bar::save_row(ui, &mut self.recording_name, "0:12 · 2 checkpoints · 34 inputs");
                    });
                    state(ui, "Deleting a recording (second step)");
                    bar::delete_confirm(ui, "Example recording");
                });
            });
        });
    }

    fn colours(&self, ui: &mut egui::Ui) {
        section(ui, "Colour", |ui| {
            let t = theme::tokens(ui);
            egui::Grid::new("gallery_colours").num_columns(6).spacing([12.0, 8.0]).show(ui, |ui| {
                for (i, (name, c)) in t.named().into_iter().enumerate() {
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(28.0, 20.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, theme::size::RADIUS, c);
                        ui.painter().rect_stroke(
                            rect,
                            theme::size::RADIUS,
                            egui::Stroke::new(1.0, t.border),
                            egui::StrokeKind::Inside,
                        );
                        ui.label(theme::caption(ui, name));
                    });
                    if i % 6 == 5 {
                        ui.end_row();
                    }
                }
            });
        });
    }
}

fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(6.0);
    theme::section_label(ui, title);
    body(ui);
}

/// `psconnector-view --gallery`
pub(crate) fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: {
            let v = egui::ViewportBuilder::default()
                .with_title("PsConnector · component gallery")
                .with_inner_size([1100.0, 820.0]);
            match crate::app::app_icon() {
                Some(icon) => v.with_icon(icon),
                None => v,
            }
        },
        ..Default::default()
    };
    eframe::run_native(
        "psconnector-gallery",
        options,
        Box::new(|cc| {
            crate::app::setup_fonts(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(App(Gallery::new())))
        }),
    )
    .map_err(|e| anyhow::anyhow!("window error: {e}"))
}

struct App(Gallery);

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.0.ui(ui);
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::{Harness, SnapshotOptions};

    use super::*;

    fn harness(dark: bool) -> Harness<'static, Gallery> {
        Harness::builder()
            .with_size(egui::vec2(1100.0, 1480.0))
            .with_accessibility_check(false)
            .with_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light })
            .with_context_setup(|ctx| {
                crate::app::setup_fonts(ctx);
                theme::apply(ctx);
            })
            .build_ui_state(|ui, g: &mut Gallery| g.ui(ui), Gallery::new())
    }

    /// Every component and state lays out without a PS5 or a GPU, in both themes.
    #[test]
    fn gallery_lays_out() {
        for dark in [false, true] {
            harness(dark).run();
        }
    }

    /// The gallery as an image in `design/screens/`, compared with the saved one. It renders with the GPU,
    /// so it is not part of the normal run:
    /// `cargo test -p psconnector-view gallery -- --ignored` compares,
    /// `UPDATE_SNAPSHOTS=1 cargo test -p psconnector-view gallery -- --ignored` saves new images.
    fn snapshot(dark: bool, name: &str) {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../design/screens");
        let mut h = harness(dark);
        h.run();
        h.snapshot_options(name, &SnapshotOptions::new().output_path(dir));
    }

    #[test]
    #[ignore = "renders with the GPU; run with --ignored"]
    fn gallery_snapshot_light() {
        snapshot(false, "gallery-light");
    }

    #[test]
    #[ignore = "renders with the GPU; run with --ignored"]
    fn gallery_snapshot_dark() {
        snapshot(true, "gallery-dark");
    }
}
