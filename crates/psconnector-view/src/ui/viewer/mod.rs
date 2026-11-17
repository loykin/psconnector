//! The streaming window: video, the bar and the picture controls.

mod bar;
mod picture_panel;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use psconnector::Frame;

use crate::app::QUIT;
use crate::macros::{Runner, record};
use crate::session::Shared;
use crate::ui::theme;
use crate::video::{gpu, picture};

pub(crate) struct Viewer {
    pub(crate) shared: Arc<Shared>,
    /// WGPU shader path; the texture is retained as a fallback for non-WGPU backends.
    pub(crate) gpu_video: Option<gpu::GpuVideo>,
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
    pub(crate) fn new(shared: Arc<Shared>, gpu_video: Option<gpu::GpuVideo>, runner: Arc<Mutex<Runner>>) -> Self {
        Self {
            shared,
            gpu_video,
            texture: None,
            shown: None,
            runner,
            fps: FpsMeter::default(),
            lut: None,
            picture_open: false,
            preset_name: String::new(),
            upload: (0, Duration::ZERO, None),
            rec_save: None,
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
