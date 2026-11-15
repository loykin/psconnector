//! The streaming window: video, the bar and the picture controls.

#[allow(unused_imports)]
use super::*;

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
    pub(crate) fn bar(&mut self, ui: &mut egui::Ui) {
        // the bar holds many controls: tighter than the rest of the window
        ui.spacing_mut().button_padding = egui::vec2(8.0, 3.0);
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.add_space(2.0);
        let runner = self.runner.clone();
        let mut r = runner.lock().unwrap();
        let r = &mut *r;
        ui.horizontal(|ui| {
            if theme::ghost(ui, true, "🎨 Picture")
                .on_hover_text("Brightness, contrast, colour range and saved game presets")
                .clicked()
            {
                self.picture_open = !self.picture_open;
            }
            let names: Vec<String> = match &r.macros {
                Ok(list) => list.iter().map(|m| m.name.clone()).collect(),
                Err(_) => Vec::new(),
            };
            let idle = r.running.is_none();
            let waiting = r.scheduled.is_some();
            ui.add_enabled_ui(idle && !waiting && !names.is_empty(), |ui| {
                let mut selected = r.selected;
                egui::ComboBox::from_id_salt("macro")
                    .selected_text(names.get(selected).cloned().unwrap_or_else(|| "no macros".into()))
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        for (i, n) in names.iter().enumerate() {
                            ui.selectable_value(&mut selected, i, n);
                        }
                    });
                if selected != r.selected {
                    r.select(selected);
                }
                ui.add(
                    egui::DragValue::new(&mut r.plan.limit_min)
                        .range(0..=24 * 60)
                        .custom_formatter(|v, _| if v < 1.0 { "no limit".into() } else { format!("{v:.0} min") }),
                )
                .on_hover_text(
                    "Run time limit (drag or type, 0 = no limit). When it is reached, a macro that checks \
                     ps.should_stop() gets 2 minutes to finish; others are stopped right away.",
                );
                egui::ComboBox::from_id_salt("after")
                    .selected_text(r.plan.after.label())
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for a in [After::Nothing, After::Standby] {
                            ui.selectable_value(&mut r.plan.after, a, a.label());
                        }
                    })
                    .response
                    .on_hover_text("What to do when the macro ends by itself or at its time limit (not on ■ Stop)");
            });
            if !idle {
                if theme::destructive(ui, true, "■ Stop").clicked() {
                    r.stop();
                }
            } else if waiting {
                if ui.button("Cancel schedule").clicked() {
                    r.cancel_schedule();
                }
            } else {
                if theme::primary(ui, !names.is_empty(), "▶ Run").clicked() {
                    r.start();
                }
                ui.add(egui::TextEdit::singleline(&mut r.at_text).hint_text("HH:MM").desired_width(60.0))
                    .on_hover_text("Start time: HH:MM (today, or tomorrow if already past) or +MINUTES");
                if ui
                    .add_enabled(!names.is_empty() && !r.at_text.trim().is_empty(), egui::Button::new("⏰ Schedule"))
                    .clicked()
                {
                    r.schedule_selected();
                }
                if theme::ghost(ui, true, "Reload list").clicked() {
                    r.reload();
                }
            }
            let status = match (&r.running, &r.scheduled, &r.macros) {
                (Some(run), _, _) => {
                    let mut s = format!("{} running {}", run.name, hms(run.started.elapsed()));
                    if let Some(d) = run.deadline {
                        s += &format!(" · {} left", hms(d.saturating_duration_since(Instant::now())));
                    }
                    if run.plan.after == After::Standby {
                        s += " · then rest mode";
                    }
                    if run.stop_sent.is_some() {
                        s += " (stopping)";
                    } else if run.wind_down.is_some() {
                        s += " (finishing)";
                    }
                    s
                }
                (None, Some(sc), _) => {
                    let name = names.get(sc.index).cloned().unwrap_or_default();
                    let wait = sc.at.duration_since(SystemTime::now()).unwrap_or_default();
                    format!("{name} starts at {} (in {})", sc.label, hms(wait))
                }
                (None, None, Err(e)) => e.clone(),
                (None, None, Ok(_)) => {
                    r.last_exit.clone().map(|e| format!("stopped ({e})")).unwrap_or_else(|| "idle".into())
                }
            };
            ui.label(status);
            let last = r.log.lock().unwrap().back().cloned().unwrap_or_default();
            ui.add(egui::Label::new(egui::RichText::new(last).weak()).truncate());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::ghost(ui, true, "Rest mode")
                    .on_hover_text("Put the PS5 into rest mode (this also disconnects)")
                    .clicked()
                {
                    r.stop();
                    self.shared.client().standby();
                    self.shared.leave.store(true, Ordering::SeqCst);
                }
                if theme::ghost(ui, true, "Disconnect")
                    .on_hover_text("Close the connection and go back to the start screen")
                    .clicked()
                {
                    self.shared.leave.store(true, Ordering::SeqCst);
                }
                if self.shared.sound {
                    let muted = self.shared.muted.load(Ordering::Relaxed);
                    if ui.button(if muted { "🔇" } else { "🔊" }).on_hover_text("Mute / unmute the PS5 sound").clicked()
                    {
                        self.shared.muted.store(!muted, Ordering::Relaxed);
                    }
                }
                let current = *self.shared.quality.lock().unwrap();
                let mut chosen = current;
                let busy =
                    self.shared.reconnecting.lock().unwrap().as_deref().is_some_and(|n| n.starts_with("reconnecting"));
                ui.add_enabled_ui(!busy, |ui| {
                    egui::ComboBox::from_id_salt("quality")
                        .selected_text(current.label())
                        .show_ui(ui, |ui| {
                            for q in Quality::ALL {
                                ui.selectable_value(&mut chosen, q, q.label());
                            }
                        })
                        .response
                        .on_hover_text(
                            "Picture quality. Changing it reconnects (a few seconds). Remembered for next time.",
                        );
                });
                if chosen != current {
                    let _ = self.shared.set_quality(chosen);
                }
                if let Some(f) = &self.shown {
                    ui.label(egui::RichText::new(format!("{}x{} {} fps", f.width, f.height, self.fps.fps)).weak());
                }
                let pad = self.shared.pad.lock().unwrap().clone();
                match pad {
                    Some(name) => ui.label(format!("🎮 {name}")),
                    None => ui.label(egui::RichText::new("🎮 no controller").weak()),
                };
            });
        });
    }
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

    pub(crate) fn picture_window(&mut self, ctx: &egui::Context) {
        let mut open = self.picture_open;
        let mut p = *self.shared.picture.lock().unwrap();
        let before = p;
        egui::Window::new("Picture").open(&mut open).resizable(false).default_width(300.0).show(ctx, |ui| {
            ui.label(theme::muted(ui, "Changes only what you see here; macros and screenshots get the original."));
            let current_codec = *self.shared.video_codec.lock().unwrap();
            let mut chosen_codec = current_codec;
            ui.horizontal(|ui| {
                ui.label("Stream codec");
                egui::ComboBox::from_id_salt("video_codec").selected_text(video_codec_name(current_codec)).show_ui(
                    ui,
                    |ui| {
                        ui.selectable_value(&mut chosen_codec, VideoCodec::H264, "H.264 SDR");
                        ui.selectable_value(&mut chosen_codec, VideoCodec::H265, "H.265 SDR");
                        ui.selectable_value(&mut chosen_codec, VideoCodec::H265Hdr, "H.265 HDR (tone-mapped)");
                    },
                );
            });
            if chosen_codec != current_codec {
                let _ = self.shared.set_video_codec(chosen_codec);
            }
            let current_range = *self.shared.video_range.lock().unwrap();
            let mut chosen_range = current_range;
            ui.horizontal(|ui| {
                ui.label("Source range");
                egui::ComboBox::from_id_salt("video_range").selected_text(video_range_name(current_range)).show_ui(
                    ui,
                    |ui| {
                        ui.selectable_value(&mut chosen_range, VideoRange::Auto, "auto");
                        ui.selectable_value(&mut chosen_range, VideoRange::Limited, "limited");
                        ui.selectable_value(&mut chosen_range, VideoRange::Full, "full");
                    },
                );
            });
            if chosen_range != current_range {
                self.shared.set_video_range(chosen_range);
            }
            let presets = Self::picture_presets();
            let current = presets
                .iter()
                .find(|(_, v, range)| *v == p && range.is_none_or(|r| r == current_range))
                .map(|(n, _, _)| n.clone())
                .unwrap_or_else(|| "Custom".into());
            egui::ComboBox::from_id_salt("picture_preset").selected_text(&current).width(220.0).show_ui(ui, |ui| {
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
            ui.add_space(4.0);
            egui::Grid::new("picture_sliders").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Brightness");
                ui.add(egui::Slider::new(&mut p.brightness, -0.5..=0.5)).on_hover_text("Overall exposure");
                ui.end_row();
                ui.label("Shadows");
                ui.add(egui::Slider::new(&mut p.shadows, 0.0..=1.0)).on_hover_text("Lift dark areas");
                ui.end_row();
                ui.label("Highlights");
                ui.add(egui::Slider::new(&mut p.highlights, 0.0..=1.0)).on_hover_text("Pull bright areas down");
                ui.end_row();
                ui.label("Contrast");
                ui.add(egui::Slider::new(&mut p.contrast, -0.5..=0.5));
                ui.end_row();
                ui.label("Saturation");
                ui.add(egui::Slider::new(&mut p.saturation, 0.0..=1.5));
                ui.end_row();
                ui.label("Warmth");
                ui.add(egui::Slider::new(&mut p.warmth, -1.0..=1.0))
                    .on_hover_text("Less blue (right) or more blue (left)");
                ui.end_row();
                ui.label("Shadow tint");
                ui.add(egui::Slider::new(&mut p.shadow_tint, 0.0..=1.0))
                    .on_hover_text("Take the colour cast out of dark areas");
                ui.end_row();
            });
            ui.horizontal(|ui| {
                if ui.button("Reset all").clicked() {
                    p = picture::Picture::default();
                    self.shared.set_video_range(VideoRange::Auto);
                }
                ui.add(egui::TextEdit::singleline(&mut self.preset_name).desired_width(120.0).hint_text("preset name"));
                if ui.add_enabled(!self.preset_name.trim().is_empty(), egui::Button::new("Save")).clicked() {
                    Self::save_preset(self.preset_name.trim(), p, *self.shared.video_range.lock().unwrap());
                    self.preset_name.clear();
                }
                let can_delete = current != "Original" && current != "Custom";
                if ui.add_enabled(can_delete, egui::Button::new("Delete")).clicked() {
                    Self::delete_preset(&current);
                }
            });
        });
        self.picture_open = open;
        if p != before {
            self.shared.set_picture(p);
        }
    }

    pub(crate) fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let quit = QUIT.load(Ordering::SeqCst);
        if quit || ctx.input(|i| i.key_pressed(egui::Key::Q) && !i.modifiers.any()) {
            self.runner.lock().unwrap().shutdown();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        egui::Panel::top("bar").show(ui, |ui| self.bar(ui));

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
        if self.picture_open {
            self.picture_window(&ctx);
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
