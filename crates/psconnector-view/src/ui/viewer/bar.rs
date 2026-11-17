//! The bar above the video: picture, the macro widget, status, quality, sound and the ⋯ menu.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;

use crate::macros::record;
use crate::macros::{After, Plan, Runner, hms};
use crate::quality::Quality;
use crate::ui::theme;
use crate::ui::theme::icon;

use super::{RecSave, Viewer};

impl Viewer {
    /// The bar keeps only what is used while playing (like an IDE's main toolbar): picture, one macro widget
    /// whose contents follow its state, the stream's status, quality, sound and a ⋯ menu for rare or
    /// disruptive actions.
    pub(crate) fn bar(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.x = 4.0;
        let runner = self.runner.clone();
        let mut r = runner.lock().unwrap();
        let r = &mut *r;
        ui.horizontal(|ui| {
            ui.set_min_height(theme::size::CONTROL);
            if theme::toggle_button(ui, icon::PICTURE, "Picture", self.picture_open)
                .on_hover_text("Brightness, contrast, colour range and saved game presets")
                .clicked()
            {
                self.picture_open = !self.picture_open;
            }
            theme::vsep(ui);
            if self.shared.recorder.recording() {
                self.recording_widget(ui);
            } else if self.rec_save.is_some() {
                self.save_widget(ui, r);
            } else if macro_widget(ui, r, self.shared.pad.lock().unwrap().is_some()) {
                self.shared.recorder.start();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.more_menu(ui, r);
                if self.shared.sound {
                    let muted = self.shared.muted.load(Ordering::Relaxed);
                    let glyph = if muted { icon::SPEAKER_OFF } else { icon::SPEAKER };
                    if theme::icon_button(ui, glyph, if muted { "Unmute the PS5 sound" } else { "Mute the PS5 sound" })
                        .clicked()
                    {
                        self.shared.muted.store(!muted, Ordering::Relaxed);
                    }
                }
                self.quality_combo(ui);
                ui.add_space(4.0);
                let pad = self.shared.pad.lock().unwrap().clone();
                let mut info = format!("{}  {}", icon::GAMEPAD, pad.as_deref().unwrap_or("no controller"));
                if let Some(f) = &self.shown {
                    info += &format!(" · {}p{}", f.height, self.fps.fps);
                }
                ui.add(egui::Label::new(theme::caption(ui, info)).truncate()).on_hover_text(match &self.shown {
                    Some(f) => format!("{}×{} at {} frames per second", f.width, f.height, self.fps.fps),
                    None => "no picture yet".into(),
                });
            });
        });
    }

    fn quality_combo(&self, ui: &mut egui::Ui) {
        let current = *self.shared.quality.lock().unwrap();
        let mut chosen = current;
        let busy = self.shared.reconnecting.lock().unwrap().as_deref().is_some_and(|n| n.starts_with("reconnecting"));
        ui.add_enabled_ui(!busy, |ui| {
            egui::ComboBox::from_id_salt("quality")
                .selected_text(current.label())
                .width(90.0)
                .show_ui(ui, |ui| {
                    for q in Quality::ALL {
                        ui.selectable_value(&mut chosen, q, q.label());
                    }
                })
                .response
                .on_hover_text("Picture quality. Changing it reconnects (a few seconds). Remembered for next time.");
        });
        if chosen != current {
            let _ = self.shared.set_quality(chosen);
        }
    }

    /// While recording: time, checkpoints, a Checkpoint button (or C) and Stop.
    fn recording_widget(&mut self, ui: &mut egui::Ui) {
        let t = theme::tokens(ui);
        let (secs, checkpoints, inputs) = self.shared.recorder.progress().unwrap_or_default();
        let info = format!(
            "{} · {checkpoints} checkpoint{} · {inputs} inputs",
            hms(Duration::from_secs_f64(secs)),
            if checkpoints == 1 { "" } else { "s" }
        );
        let (mark, stop) = state_strip(ui, t.danger_soft, t.danger, |ui| {
            theme::status_dot(ui, t.danger);
            ui.label("Recording");
            ui.label(egui::RichText::new(info).color(t.muted));
            let mark = ui
                .add(
                    egui::Button::new(format!("{}  Checkpoint", icon::FLAG))
                        .frame_when_inactive(false)
                        .min_size(egui::vec2(0.0, theme::size::CONTROL)),
                )
                .on_hover_text(
                    "Press when the screen shows something the replay should wait for (a menu, the end of a \
                     load, a place). The replay waits here until the screen looks like this again. Key: C",
                )
                .clicked();
            (mark, end_button(ui, icon::STOP, t.danger, true).on_hover_text("Stop recording").clicked())
        });
        if mark && let Some(f) = self.shared.client().frame() {
            self.shared.recorder.checkpoint(&f);
        }
        if stop {
            self.stop_recording();
        }
    }

    pub(crate) fn stop_recording(&mut self) {
        if let Some(take) = self.shared.recorder.stop() {
            let name = format!("Recording {}", chrono::Local::now().format("%m-%d %H:%M"));
            self.rec_save = Some(RecSave { take, name, error: None });
        }
    }

    /// After recording: name it, Save or Discard (how often it repeats is set in the macro options).
    fn save_widget(&mut self, ui: &mut egui::Ui, r: &mut Runner) {
        let Some(s) = self.rec_save.as_mut() else { return };
        let t = theme::tokens(ui);
        let mut save = false;
        let mut discard = false;
        ui.label(egui::RichText::new(icon::RECORD).color(t.danger));
        let resp = ui.add(theme::field(&mut s.name, theme::size::CONTROL).desired_width(170.0));
        save |= resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        save |= theme::primary(ui, true, &format!("{}  Save", icon::SAVE)).clicked();
        discard |= theme::ghost(ui, true, "Discard").clicked();
        let summary = format!(
            "{} · {} checkpoints · {} inputs",
            hms(Duration::from_secs_f64(s.take.duration())),
            s.take.checkpoints(),
            s.take.inputs()
        );
        ui.label(theme::caption(ui, s.error.clone().unwrap_or(summary)));
        if discard {
            self.rec_save = None;
        } else if save {
            let folder = r.sources.iter().find(|p| p.is_dir()).cloned();
            let result = match folder {
                Some(dir) => record::save(&s.take, &dir, &s.name).map_err(|e| format!("{e:#}")),
                None => Err("no macro folder to save into".to_string()),
            };
            match result {
                Ok(dir) => {
                    log::info!("recording saved to {}", dir.display());
                    let name = s.name.trim().to_string();
                    r.reload();
                    if let Ok(i) = r.find(&name) {
                        r.select(i);
                    }
                    self.rec_save = None;
                }
                Err(e) => s.error = Some(e),
            }
        }
    }

    /// Actions that are rare or end the session.
    fn more_menu(&self, ui: &mut egui::Ui, r: &mut Runner) {
        let t = theme::tokens(ui);
        let button = egui::Button::new(egui::RichText::new(icon::MORE).size(16.0))
            .frame_when_inactive(false)
            .min_size(egui::vec2(theme::size::CONTROL, theme::size::CONTROL));
        let (resp, _) = egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
            ui.set_min_width(200.0);
            if ui.button(format!("{}  Reload macro list", icon::RELOAD)).clicked() {
                r.reload();
            }
            ui.separator();
            if ui
                .button(format!("{}  Disconnect", icon::DISCONNECT))
                .on_hover_text("Close the connection and go back to the start screen")
                .clicked()
            {
                self.shared.leave.store(true, Ordering::SeqCst);
            }
            let rest = egui::RichText::new(format!("{}  Rest mode", icon::POWER)).color(t.danger_text);
            if ui.button(rest).on_hover_text("Put the PS5 into rest mode (this also disconnects)").clicked() {
                r.stop();
                self.shared.client().standby();
                self.shared.leave.store(true, Ordering::SeqCst);
            }
        });
        resp.on_hover_text("More");
    }
}

/// Corner radius for the left or right end of a joined control.
fn end_radius(left: bool) -> egui::CornerRadius {
    let r = theme::size::RADIUS;
    if left {
        egui::CornerRadius { nw: r, sw: r, ne: 0, se: 0 }
    } else {
        egui::CornerRadius { nw: 0, sw: 0, ne: r, se: r }
    }
}

/// A square button at the right end of a joined control.
fn end_button(ui: &mut egui::Ui, glyph: &str, fill: egui::Color32, enabled: bool) -> egui::Response {
    let b = egui::Button::new(egui::RichText::new(glyph).size(14.0).color(egui::Color32::WHITE))
        .fill(fill)
        .stroke(egui::Stroke::NONE)
        .corner_radius(end_radius(false))
        .min_size(egui::vec2(30.0, theme::size::CONTROL));
    ui.add_enabled(enabled, b)
}

/// A framed strip for the running and scheduled states of the macro widget.
fn state_strip<R>(
    ui: &mut egui::Ui,
    fill: egui::Color32,
    stroke: egui::Color32,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    egui::Frame::new()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke))
        .corner_radius(egui::CornerRadius::same(theme::size::RADIUS))
        .inner_margin(egui::Margin { left: 10, right: 0, top: 0, bottom: 0 })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                body(ui)
            })
            .inner
        })
        .inner
}

/// One widget for macros, like an IDE's run widget: what it shows depends on whether a macro is running,
/// scheduled or idle, so the three never compete for space. Returns true when Record was pressed.
fn macro_widget(ui: &mut egui::Ui, r: &mut Runner, has_pad: bool) -> bool {
    let t = theme::tokens(ui);
    let names: Vec<String> = match &r.macros {
        Ok(list) => list.iter().map(|m| m.name.clone()).collect(),
        Err(_) => Vec::new(),
    };
    if let Some(run) = &r.running {
        let mut times = hms(run.started.elapsed());
        if let Some(d) = run.deadline {
            times += &format!(" · {} left", hms(d.saturating_duration_since(Instant::now())));
        }
        if run.plan.after == After::Standby {
            times += " · then rest";
        }
        if run.stop_sent.is_some() {
            times += " · stopping";
        } else if run.wind_down.is_some() {
            times += " · finishing";
        }
        let name = run.name.clone();
        let stop = state_strip(ui, t.accent_soft, t.accent, |ui| {
            theme::status_dot(ui, t.success);
            ui.label(&name);
            ui.label(egui::RichText::new(times).color(t.muted));
            end_button(ui, icon::STOP, t.danger, true).on_hover_text("Stop the macro").clicked()
        });
        if stop {
            r.stop();
        }
        let last = r.log.lock().unwrap().back().cloned().unwrap_or_default();
        ui.add_space(6.0);
        ui.add(egui::Label::new(egui::RichText::new(last).size(theme::size::CAPTION).color(t.faint)).truncate());
        return false;
    }
    if let Some(sc) = &r.scheduled {
        let name = names.get(sc.index).cloned().unwrap_or_default();
        let wait = sc.at.duration_since(SystemTime::now()).unwrap_or_default();
        let label = format!("{name} · {}", sc.label);
        let cancel = state_strip(ui, egui::Color32::TRANSPARENT, t.faint, |ui| {
            ui.label(egui::RichText::new(icon::CLOCK).color(t.muted));
            ui.label(label);
            ui.label(egui::RichText::new(format!("in {}", hms(wait))).color(t.muted));
            let b = egui::Button::new("Cancel")
                .frame_when_inactive(false)
                .corner_radius(end_radius(false))
                .min_size(egui::vec2(0.0, theme::size::CONTROL));
            ui.add(b).clicked()
        });
        if cancel {
            r.cancel_schedule();
        }
        return false;
    }
    let has = !names.is_empty();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_enabled_ui(has, |ui| {
            let w = &mut ui.visuals_mut().widgets;
            for v in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
                v.corner_radius = end_radius(true);
            }
            let mut selected = r.selected;
            let combo = egui::ComboBox::from_id_salt("macro")
                .selected_text(names.get(selected).cloned().unwrap_or_else(|| "no macros".into()))
                .width(180.0)
                .show_ui(ui, |ui| {
                    for (i, n) in names.iter().enumerate() {
                        ui.selectable_value(&mut selected, i, n);
                    }
                })
                .response;
            if let Err(e) = &r.macros {
                combo.on_disabled_hover_text(e.clone());
            }
            if selected != r.selected {
                r.select(selected);
            }
        });
        if end_button(ui, icon::PLAY, t.accent, has).on_hover_text("Run the macro now").clicked() {
            r.start();
        }
    });
    let button = egui::Button::new(egui::RichText::new(icon::SLIDERS).size(16.0))
        .frame_when_inactive(false)
        .min_size(egui::vec2(theme::size::CONTROL, theme::size::CONTROL));
    let options = egui::containers::menu::MenuButton::from_button(button).config(
        egui::containers::menu::MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
    );
    let recording = r.macros.as_ref().ok().and_then(|l| l.get(r.selected)).is_some_and(|m| m.recording.is_some());
    let (resp, _) =
        options.ui(ui, |ui| {
            ui.set_min_width(280.0);
            theme::section_label(ui, "Macro options");
            egui::Grid::new("macro_options").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
                if recording {
                    ui.label("Repeat");
                    ui.add(egui::DragValue::new(&mut r.plan.repeat).range(0..=100_000).speed(0.2).custom_formatter(
                        |v, _| if v < 1.0 { "until stopped".into() } else { format!("{v:.0} times") },
                    ))
                    .on_hover_text("How many times the recording plays. Drag or type; 0 = until you stop it");
                    ui.end_row();
                }
                ui.label("Stop after");
                ui.add(
                    egui::DragValue::new(&mut r.plan.limit_min)
                        .range(0..=24 * 60)
                        .custom_formatter(|v, _| if v < 1.0 { "never".into() } else { format!("{v:.0} minutes") }),
                )
                .on_hover_text(
                    "A time limit in minutes. Drag or type; 0 = never. When it is reached, a macro that checks \
                 ps.should_stop() (recordings do, after each lap) gets 2 minutes to finish; others are stopped \
                 right away.",
                );
                ui.end_row();
                ui.label("When done");
                theme::segmented(ui, &mut r.plan.after, &[(After::Nothing, "Nothing"), (After::Standby, "Rest mode")]);
                ui.end_row();
                ui.label("Start at");
                ui.horizontal(|ui| {
                    ui.add(theme::field(&mut r.at_text, theme::size::CONTROL).hint_text("HH:MM").desired_width(64.0))
                        .on_hover_text("HH:MM (today, or tomorrow if already past) or +MINUTES");
                    let can = has && !r.at_text.trim().is_empty();
                    if ui.add_enabled(can, egui::Button::new(format!("{}  Schedule", icon::CLOCK))).clicked() {
                        r.schedule_selected();
                        ui.close();
                    }
                });
                ui.end_row();
            });
            ui.separator();
            ui.label(theme::caption(ui, run_summary(&r.plan, recording)));
            if recording {
                delete_recording(ui, r);
            }
            let last = match &r.macros {
                Err(e) => e.clone(),
                Ok(_) => r.last_exit.clone().map(|e| format!("Last run: {e}")).unwrap_or_default(),
            };
            if !last.is_empty() {
                ui.separator();
                ui.label(theme::caption(ui, last));
            }
        });
    resp.on_hover_text("Repeat, time limit, what to do afterwards, start time");
    // the settings that change what ▶ does stay visible without opening the menu
    let mut summary = Vec::new();
    if recording {
        summary.push(if r.plan.repeat == 0 { "∞".to_string() } else { format!("{}×", r.plan.repeat) });
    }
    if r.plan.limit_min > 0 {
        summary.push(format!("{} min", r.plan.limit_min));
    }
    if r.plan.after == After::Standby {
        summary.push("then rest".into());
    }
    if !summary.is_empty() {
        ui.label(theme::caption(ui, summary.join(" · "))).on_hover_text(run_summary(&r.plan, recording));
    }
    theme::vsep(ui);
    let rec = ui
        .add_enabled(
            has_pad,
            egui::Button::new(format!("{}  Record", icon::RECORD))
                .frame_when_inactive(false)
                .min_size(egui::vec2(0.0, theme::size::CONTROL)),
        )
        .on_hover_text(
            "Record what you do with the controller as a new macro. Mark checkpoints while recording (C) \
             where the replay should wait for the screen.",
        )
        .on_disabled_hover_text("Connect a controller to this computer to record");
    rec.clicked()
}

/// Delete for a recording, with a second click to confirm.
fn delete_recording(ui: &mut egui::Ui, r: &mut Runner) {
    let t = theme::tokens(ui);
    let id = ui.scope_id().with("delete_recording_confirm");
    let name = r.macros.as_ref().ok().and_then(|l| l.get(r.selected)).map(|m| m.name.clone()).unwrap_or_default();
    // the confirmation belongs to one macro: picking another one cancels it
    let confirming = ui.data(|d| d.get_temp::<String>(id)).is_some_and(|n| n == name);
    ui.separator();
    if !confirming {
        let b =
            egui::Button::new(egui::RichText::new(format!("{}  Delete recording", icon::TRASH)).color(t.danger_text))
                .frame_when_inactive(false);
        if ui.add(b).clicked() {
            ui.data_mut(|d| d.insert_temp(id, name));
        }
        return;
    }
    let where_to = if cfg!(target_os = "macos") { "It goes to the Trash." } else { "This cannot be undone." };
    ui.label(theme::caption(ui, format!("Delete “{name}”? {where_to}")));
    ui.horizontal(|ui| {
        if theme::destructive(ui, true, "Delete").clicked() {
            ui.data_mut(|d| d.remove::<String>(id));
            match r.delete_selected() {
                Ok(msg) => r.last_exit = Some(msg),
                Err(e) => r.last_exit = Some(format!("Not deleted: {e}")),
            }
        }
        if theme::ghost(ui, true, "Cancel").clicked() {
            ui.data_mut(|d| d.remove::<String>(id));
        }
    });
}

/// What ▶ will do with these options, in one sentence.
fn run_summary(plan: &Plan, recording: bool) -> String {
    let what = match (recording, plan.repeat) {
        (false, _) => "Runs the macro".to_string(),
        (true, 0) => "Plays the recording over and over".to_string(),
        (true, 1) => "Plays the recording once".to_string(),
        (true, n) => format!("Plays the recording {n} times"),
    };
    let until = match (recording && plan.repeat > 0, plan.limit_min) {
        (_, 0) if recording && plan.repeat == 0 => " until you stop it".to_string(),
        (_, 0) => String::new(),
        (true, m) => format!(", or stops after {m} minutes if that comes first"),
        (false, m) => format!(" for at most {m} minutes"),
    };
    let after = if plan.after == After::Standby { ", then puts the PS5 in rest mode" } else { "" };
    format!("{what}{until}{after}.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_summary_says_what_play_will_do() {
        let plan = |repeat, limit_min, after| Plan { repeat, limit_min, after };
        assert_eq!(
            run_summary(&plan(0, 0, After::Nothing), true),
            "Plays the recording over and over until you stop it."
        );
        assert_eq!(
            run_summary(&plan(5, 30, After::Standby), true),
            "Plays the recording 5 times, or stops after 30 minutes if that comes first, then puts the PS5 in rest mode."
        );
        assert_eq!(
            run_summary(&plan(0, 45, After::Nothing), true),
            "Plays the recording over and over for at most 45 minutes."
        );
        assert_eq!(run_summary(&plan(0, 0, After::Nothing), false), "Runs the macro.");
    }
}
