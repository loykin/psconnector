//! Look of the window, after shadcn/ui's default "zinc" theme: neutral greys, thin borders, rounded
//! corners, roomy spacing. Follows the system light / dark setting.

use eframe::egui::{self, Color32, CornerRadius, Stroke, Theme, Visuals};

const RADIUS: u8 = 6;

/// One palette (shadcn CSS variables).
struct Palette {
    background: Color32,
    foreground: Color32,
    muted: Color32,
    muted_foreground: Color32,
    border: Color32,
    /// hover / pressed surfaces
    accent: Color32,
    primary: Color32,
    primary_foreground: Color32,
    destructive: Color32,
    ring: Color32,
}

const LIGHT: Palette = Palette {
    background: Color32::from_rgb(255, 255, 255),
    foreground: Color32::from_rgb(9, 9, 11),            // zinc-950
    muted: Color32::from_rgb(244, 244, 245),            // zinc-100
    muted_foreground: Color32::from_rgb(113, 113, 122), // zinc-500
    border: Color32::from_rgb(228, 228, 231),           // zinc-200
    accent: Color32::from_rgb(244, 244, 245),
    primary: Color32::from_rgb(24, 24, 27), // zinc-900
    primary_foreground: Color32::from_rgb(250, 250, 250),
    destructive: Color32::from_rgb(220, 38, 38), // red-600
    ring: Color32::from_rgb(161, 161, 170),      // zinc-400
};

const DARK: Palette = Palette {
    background: Color32::from_rgb(9, 9, 11),
    foreground: Color32::from_rgb(250, 250, 250),
    muted: Color32::from_rgb(39, 39, 42), // zinc-800
    muted_foreground: Color32::from_rgb(161, 161, 170),
    border: Color32::from_rgb(39, 39, 42),
    accent: Color32::from_rgb(39, 39, 42),
    primary: Color32::from_rgb(250, 250, 250),
    primary_foreground: Color32::from_rgb(24, 24, 27),
    destructive: Color32::from_rgb(239, 68, 68), // red-500
    ring: Color32::from_rgb(113, 113, 122),
};

fn palette(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

fn visuals(p: &Palette, dark: bool) -> Visuals {
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    let r = CornerRadius::same(RADIUS);
    v.override_text_color = Some(p.foreground);
    v.panel_fill = p.background;
    v.window_fill = p.background;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(RADIUS + 2);
    v.menu_corner_radius = r;
    v.extreme_bg_color = p.background; // text fields
    v.faint_bg_color = p.muted;
    v.code_bg_color = p.muted;
    v.hyperlink_color = p.foreground;
    v.selection.bg_fill = p.ring.gamma_multiply(0.45);
    v.selection.stroke = Stroke::new(1.0, p.foreground);
    v.widgets.noninteractive.bg_fill = p.background;
    v.widgets.noninteractive.weak_bg_fill = p.background;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.foreground);
    v.widgets.noninteractive.corner_radius = r;
    for (w, fill) in [
        (&mut v.widgets.inactive, p.background),
        (&mut v.widgets.hovered, p.accent),
        (&mut v.widgets.active, p.accent),
        (&mut v.widgets.open, p.accent),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0, p.border);
        w.fg_stroke = Stroke::new(1.0, p.foreground);
        w.corner_radius = r;
        w.expansion = 0.0;
    }
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.ring);
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.ring);
    v
}

/// Installs the theme for both light and dark, and follows the system setting.
pub fn apply(ctx: &egui::Context) {
    for (theme, p, dark) in [(Theme::Light, &LIGHT, false), (Theme::Dark, &DARK, true)] {
        ctx.style_mut_of(theme, |s| {
            s.visuals = visuals(p, dark);
            s.spacing.item_spacing = egui::vec2(8.0, 8.0);
            s.spacing.button_padding = egui::vec2(12.0, 5.0);
            s.spacing.interact_size.y = 30.0;
            s.spacing.combo_width = 120.0;
            for (style, size) in [
                (egui::TextStyle::Body, 14.0),
                (egui::TextStyle::Button, 14.0),
                (egui::TextStyle::Small, 12.0),
                (egui::TextStyle::Heading, 22.0),
            ] {
                if let Some(f) = s.text_styles.get_mut(&style) {
                    f.size = size;
                }
            }
        });
    }
    ctx.set_theme(egui::ThemePreference::System);
}

/// A bordered card (shadcn `Card`) with a title and an optional description.
pub fn card<R>(ui: &mut egui::Ui, title: &str, description: Option<&str>, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let p = palette(ui);
    egui::Frame::new()
        .fill(p.background)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(RADIUS + 2))
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(title).strong().size(16.0));
            if let Some(d) = description {
                ui.label(muted(ui, d));
            }
            ui.add_space(4.0);
            body(ui)
        })
        .inner
}

/// Secondary text.
pub fn muted(ui: &egui::Ui, text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).color(palette(ui).muted_foreground)
}

/// Main action of a screen or row (dark button).
pub fn primary(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    let p = palette(ui);
    let b =
        egui::Button::new(egui::RichText::new(text).color(p.primary_foreground)).fill(p.primary).stroke(Stroke::NONE);
    ui.add_enabled(enabled, b)
}

/// Something that removes data (red button).
pub fn destructive(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    let p = palette(ui);
    let b = egui::Button::new(egui::RichText::new(text).color(Color32::WHITE)).fill(p.destructive).stroke(Stroke::NONE);
    ui.add_enabled(enabled, b)
}

/// Quiet button without a border (shadcn `ghost`).
pub fn ghost(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    ui.add_enabled(enabled, egui::Button::new(text).frame(false))
}

/// Small rounded label (shadcn `Badge`).
pub fn badge(ui: &mut egui::Ui, text: &str, strong: bool) {
    let p = palette(ui);
    let (fill, fg) = if strong { (p.primary, p.primary_foreground) } else { (p.muted, p.muted_foreground) };
    egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| ui.label(egui::RichText::new(text).size(12.0).color(fg)));
}

/// Error text.
pub fn error(ui: &mut egui::Ui, text: &str) {
    let p = palette(ui);
    ui.label(egui::RichText::new(text).color(p.destructive));
}
