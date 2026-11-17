//! The picture controls beside the video: presets, source codec and range, and the adjustment sliders.

use eframe::egui;
use psconnector::{VideoCodec, VideoRange};

use crate::ui::theme;
use crate::ui::theme::icon;
use crate::video::{picture, presets};

use super::Viewer;

impl Viewer {
    /// The picture controls, as a panel beside the video so the change can be watched while sliding.
    pub(crate) fn picture_panel(&mut self, ui: &mut egui::Ui) {
        let t = theme::tokens(ui);
        let mut p = *self.shared.picture.lock().unwrap();
        let before = p;
        let description = "Changes only what you see here; macros and screenshots get the original.";
        let closed = theme::side_panel(ui, "picture", 320.0, "Picture", Some(description), |ui| {
            let current_range = *self.shared.video_range.lock().unwrap();
            let presets = presets::all();
            let current = presets
                .iter()
                .find(|preset| preset.matches(&p, current_range))
                .map(|preset| preset.name.clone())
                .unwrap_or_else(|| "Custom".into());
            theme::section_label(ui, "Preset");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("picture_preset").selected_text(&current).width(190.0).show_ui(ui, |ui| {
                    for preset in &presets {
                        if ui.selectable_label(preset.matches(&p, current_range), &preset.name).clicked() {
                            p = preset.picture;
                            if let Some(range) = preset.range {
                                self.shared.set_video_range(range);
                            }
                        }
                    }
                });
                let save =
                    egui::Button::new(format!("{}  Save", icon::SAVE)).min_size(egui::vec2(0.0, theme::size::CONTROL));
                let config = egui::containers::menu::MenuConfig::new()
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
                egui::containers::menu::MenuButton::from_button(save).config(config).ui(ui, |ui| {
                    ui.set_min_width(240.0);
                    ui.label(theme::caption(ui, "Save the current picture as a preset"));
                    ui.add(
                        theme::field(&mut self.preset_name, theme::size::CONTROL)
                            .hint_text("Preset name")
                            .desired_width(f32::INFINITY),
                    );
                    ui.horizontal(|ui| {
                        if theme::primary(ui, !self.preset_name.trim().is_empty(), "Save").clicked() {
                            presets::save(self.preset_name.trim(), p, *self.shared.video_range.lock().unwrap());
                            self.preset_name.clear();
                            ui.close();
                        }
                        let can_delete = current != presets::ORIGINAL && current != "Custom";
                        if theme::ghost(ui, can_delete, &format!("{}  Delete \"{current}\"", icon::TRASH)).clicked() {
                            presets::delete(&current);
                            ui.close();
                        }
                    });
                });
            });
            ui.add_space(12.0);

            theme::section_label(ui, "Source");
            egui::Grid::new("picture_source").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Codec");
                let mut codec = *self.shared.video_codec.lock().unwrap();
                if theme::segmented(
                    ui,
                    &mut codec,
                    &[(VideoCodec::H264, "H.264"), (VideoCodec::H265, "H.265"), (VideoCodec::H265Hdr, "HDR")],
                ) {
                    let _ = self.shared.set_video_codec(codec);
                }
                ui.end_row();
                ui.label("Range");
                let mut range = current_range;
                if theme::segmented(
                    ui,
                    &mut range,
                    &[(VideoRange::Auto, "Auto"), (VideoRange::Limited, "Limited"), (VideoRange::Full, "Full")],
                ) {
                    self.shared.set_video_range(range);
                }
                ui.end_row();
            });
            ui.add_space(12.0);

            ui.horizontal(|ui| {
                theme::section_label(ui, "Adjust");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let reset = egui::Button::new(
                        egui::RichText::new("Reset all").size(theme::size::CAPTION).color(t.accent_text),
                    )
                    .frame(false);
                    if ui.add(reset).clicked() {
                        p = picture::Picture::default();
                        self.shared.set_video_range(VideoRange::Auto);
                    }
                });
            });
            let sliders: [(&str, &mut f32, std::ops::RangeInclusive<f32>, &str); 7] = [
                ("Brightness", &mut p.brightness, -0.5..=0.5, "Overall exposure"),
                ("Shadows", &mut p.shadows, 0.0..=1.0, "Lift dark areas"),
                ("Highlights", &mut p.highlights, 0.0..=1.0, "Pull bright areas down"),
                ("Contrast", &mut p.contrast, -0.5..=0.5, ""),
                ("Saturation", &mut p.saturation, 0.0..=1.5, ""),
                ("Warmth", &mut p.warmth, -1.0..=1.0, "Less blue (right) or more blue (left)"),
                ("Shadow tint", &mut p.shadow_tint, 0.0..=1.0, "Take the colour cast out of dark areas"),
            ];
            for (name, value, range, hint) in sliders {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(name);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(format!("{:+.2}", *value)).color(t.muted).monospace());
                    });
                });
                ui.spacing_mut().slider_width = ui.available_width();
                let r = ui.add(egui::Slider::new(value, range).show_value(false));
                if !hint.is_empty() {
                    r.on_hover_text(hint);
                }
            }
        });
        if closed {
            self.picture_open = false;
        }
        if p != before {
            self.shared.set_picture(p);
        }
    }
}
