//! The streaming window: video, the bar and the picture controls.

#[allow(unused_imports)]
use super::*;
use crate::theme::icon;

pub(crate) struct Viewer {
    pub(crate) shared: Arc<Shared>,
    /// WGPU shader path; the texture is retained as a fallback for non-WGPU backends.
    pub(crate) gpu_video: Option<gpu_video::GpuVideo>,
    pub(crate) texture: Option<egui::TextureHandle>,
    pub(crate) shown: Option<Arc<Frame>>,
    pub(crate) runner: Arc<Mutex<Runner>>,
    pub(crate) fps: FpsMeter,
    /// Picture adjustment: tables for the version in use and the settings window
    pub(crate) lut: Option<(u64, picture::Lut)>,
    pub(crate) picture_open: bool,
    pub(crate) preset_name: String,
    /// Time spent turning frames into the window texture, logged every 10 s
    pub(crate) upload: (u32, Duration, Option<Instant>),
    /// A finished recording waiting for a name (Save / Discard in the bar)
    pub(crate) rec_save: Option<RecSave>,
}

pub(crate) struct RecSave {
    pub(crate) take: record::Take,
    pub(crate) name: String,
    pub(crate) error: Option<String>,
}

/// Frames shown per second, over the last second.
#[derive(Default)]
pub(crate) struct FpsMeter {
    pub(crate) since: Option<Instant>,
    pub(crate) count: u32,
    pub(crate) fps: u32,
}

impl FpsMeter {
    pub(crate) fn frame(&mut self) {
        self.count += 1;
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() >= Duration::from_secs(1) {
            self.fps = self.count;
            self.count = 0;
            self.since = Some(Instant::now());
        }
    }
}

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

impl Viewer {
    /// Built-ins establish the safe default: every game is untouched unless its own profile is selected.
    pub(crate) fn builtin_presets() -> Vec<(String, picture::Picture, Option<VideoRange>)> {
        vec![("Original".into(), picture::Picture::default(), Some(VideoRange::Auto))]
    }

    pub(crate) fn save_preset(name: &str, p: picture::Picture, range: VideoRange) {
        let mut all =
            settings::get("picture_presets").filter(Value::is_object).unwrap_or_else(|| serde_json::json!({}));
        all[name] = serde_json::json!({
            "picture": p.to_json(),
            "video_range": video_range_name(range),
        });
        settings::set("picture_presets", all);
    }

    pub(crate) fn delete_preset(name: &str) -> bool {
        let Some(mut all) = settings::get("picture_presets").filter(Value::is_object) else {
            return false;
        };
        let removed = all.as_object_mut().is_some_and(|o| o.remove(name).is_some());
        if removed {
            settings::set("picture_presets", all);
        }
        removed
    }

    /// Saved presets (name → values) from the settings file.
    pub(crate) fn saved_presets() -> Vec<(String, picture::Picture, Option<VideoRange>)> {
        settings::get("picture_presets")
            .and_then(|v| v.as_object().cloned())
            .map(|o| {
                o.iter()
                    .map(|(k, v)| {
                        // Old presets stored picture fields directly. New presets also carry source range.
                        let values = v.get("picture").unwrap_or(v);
                        let range = v.get("video_range").and_then(Value::as_str).and_then(parse_video_range);
                        (k.clone(), picture::Picture::default().merged(values), range)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn picture_presets() -> Vec<(String, picture::Picture, Option<VideoRange>)> {
        Self::builtin_presets().into_iter().chain(Self::saved_presets()).collect()
    }

    /// The picture controls, as a panel beside the video so the change can be watched while sliding.
    pub(crate) fn picture_panel(&mut self, ui: &mut egui::Ui) {
        let t = theme::tokens(ui);
        let mut p = *self.shared.picture.lock().unwrap();
        let before = p;
        let description = "Changes only what you see here; macros and screenshots get the original.";
        let closed = theme::side_panel(ui, "picture", 320.0, "Picture", Some(description), |ui| {
            let current_range = *self.shared.video_range.lock().unwrap();
            let presets = Self::picture_presets();
            let current = presets
                .iter()
                .find(|(_, v, range)| *v == p && range.is_none_or(|r| r == current_range))
                .map(|(n, _, _)| n.clone())
                .unwrap_or_else(|| "Custom".into());
            theme::section_label(ui, "Preset");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("picture_preset").selected_text(&current).width(190.0).show_ui(ui, |ui| {
                    for (name, v, range) in &presets {
                        let selected = *v == p && range.is_none_or(|r| r == current_range);
                        if ui.selectable_label(selected, name).clicked() {
                            p = *v;
                            if let Some(range) = range {
                                self.shared.set_video_range(*range);
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
                            Self::save_preset(self.preset_name.trim(), p, *self.shared.video_range.lock().unwrap());
                            self.preset_name.clear();
                            ui.close();
                        }
                        let can_delete = current != "Original" && current != "Custom";
                        if theme::ghost(ui, can_delete, &format!("{}  Delete \"{current}\"", icon::TRASH)).clicked() {
                            Self::delete_preset(&current);
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

    pub(crate) fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let quit = QUIT.load(Ordering::SeqCst);
        let typing = ctx.egui_wants_keyboard_input();
        if self.shared.recorder.recording()
            && !typing
            && ctx.input(|i| i.key_pressed(egui::Key::C) && !i.modifiers.any())
            && let Some(f) = self.shared.client().frame()
        {
            self.shared.recorder.checkpoint(&f);
        }
        if quit || (!typing && ctx.input(|i| i.key_pressed(egui::Key::Q) && !i.modifiers.any())) {
            self.runner.lock().unwrap().shutdown();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        let t = theme::tokens(ui);
        let bar = egui::Frame::new().fill(t.surface).inner_margin(egui::Margin::symmetric(8, 6));
        egui::Panel::top("bar").frame(bar).show(ui, |ui| self.bar(ui));
        if self.picture_open {
            self.picture_panel(ui);
        }

        let latest = self.shared.client().frame();
        let version = self.shared.picture_version.load(Ordering::SeqCst);
        let picture_changed = self.lut.as_ref().is_none_or(|(v, _)| *v != version);
        if picture_changed {
            let p = *self.shared.picture.lock().unwrap();
            self.lut = Some((version, picture::Lut::new(&p)));
        }
        let neutral = self.shared.picture.lock().unwrap().is_neutral();
        if let Some(f) = latest
            && (picture_changed || !self.shown.as_ref().is_some_and(|s| Arc::ptr_eq(s, &f)))
        {
            let new_frame = !self.shown.as_ref().is_some_and(|s| Arc::ptr_eq(s, &f));
            let t0 = Instant::now();
            if let Some(gpu) = &self.gpu_video {
                let (_, lut) = self.lut.as_ref().expect("lut");
                gpu.submit(f.clone(), lut, version);
            } else {
                let img = if neutral {
                    egui::ColorImage::from_rgb([f.width, f.height], f.rgb())
                } else {
                    let (_, lut) = self.lut.as_ref().expect("lut");
                    egui::ColorImage::new([f.width, f.height], lut.apply_color32(f.rgb()))
                };
                match &mut self.texture {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => self.texture = Some(ctx.load_texture("ps5", img, egui::TextureOptions::LINEAR)),
                }
            }
            self.shown = Some(f);
            if new_frame {
                self.fps.frame();
            }
            let (n, total, since) = &mut self.upload;
            *n += 1;
            *total += t0.elapsed();
            let since = *since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_secs(10) {
                if let Some(gpu) = &self.gpu_video {
                    let (prepared, ms) = gpu.take_prepare_stats();
                    log::info!(
                        "window: {} frames shown in 10 s, {} GPU uploads ({:.2} ms CPU each; picture adjustment on GPU)",
                        *n,
                        prepared,
                        ms,
                    );
                } else {
                    log::info!(
                        "window: {} frames shown in 10 s, {:.1} ms each to adjust and upload",
                        *n,
                        total.as_secs_f64() * 1000.0 / f64::from((*n).max(1))
                    );
                }
                self.upload = (0, Duration::ZERO, Some(Instant::now()));
            }
        }
        // draw into the remaining area at 16:9
        let area = ui.available_rect_before_wrap();
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 0.0, egui::Color32::BLACK);
        let size = if area.width() / area.height() > 16.0 / 9.0 {
            egui::vec2(area.height() * 16.0 / 9.0, area.height())
        } else {
            egui::vec2(area.width(), area.width() * 9.0 / 16.0)
        };
        let rect = egui::Rect::from_center_size(area.center(), size);
        if let Some(gpu) = &self.gpu_video
            && self.shown.is_some()
        {
            painter.add(gpu.paint_callback(rect));
        } else if let Some(t) = &self.texture {
            painter.image(
                t.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        let mut text = self.shared.osd.lock().unwrap().clone();
        let note = self.shared.reconnecting.lock().unwrap().clone();
        if let Some(note) = note {
            text = format!("{note}\n{text}");
        } else if !self.shared.client().is_ready() {
            text = format!("disconnected: {:?}\n{text}", self.shared.client().phase());
        }
        if !text.is_empty() {
            let font = egui::FontId::proportional(20.0);
            let galley = painter.layout_no_wrap(text, font, egui::Color32::from_rgb(255, 220, 120));
            let pos = rect.min + egui::vec2(12.0, 10.0);
            painter.rect_filled(
                egui::Rect::from_min_size(pos, galley.size()).expand(6.0),
                4.0,
                egui::Color32::from_black_alpha(200),
            );
            painter.galley(pos, galley, egui::Color32::WHITE);
        }
        // Frames wake the window immediately; this slower tick is for OSD, connection state and controls.
        ctx.request_repaint_after(Duration::from_millis(100));
    }
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
