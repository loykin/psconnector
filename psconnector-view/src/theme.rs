//! The viewer's design system: tokens (colours, sizes, type), icons and the few components every screen is
//! built from. Screens use only these, so the look stays consistent and is the same on macOS and Windows.
//!
//! Follows the system light / dark setting; the streaming window is dark by nature, so dark is the
//! reference palette.

use eframe::egui::{self, Color32, CornerRadius, Margin, Response, RichText, Stroke, Theme, Ui, Visuals};

/// Phosphor (regular) icon glyphs, from `assets/Phosphor.ttf`. Icons are text, so they take the text colour.
pub mod icon {
    pub const PLAY: &str = "\u{E3D0}";
    pub const STOP: &str = "\u{E46C}";
    pub const SLIDERS: &str = "\u{E434}";
    pub const MORE: &str = "\u{E1FE}";
    pub const SPEAKER: &str = "\u{E44A}";
    pub const SPEAKER_OFF: &str = "\u{E45C}";
    pub const GAMEPAD: &str = "\u{E26E}";
    pub const PICTURE: &str = "\u{E18C}";
    pub const CLOCK: &str = "\u{E19A}";
    pub const RELOAD: &str = "\u{E036}";
    pub const POWER: &str = "\u{E3DA}";
    pub const DISCONNECT: &str = "\u{E42A}";
    pub const WARNING: &str = "\u{E4E2}";
    pub const USER: &str = "\u{E4C2}";
    pub const CLOSE: &str = "\u{E4F6}";
    pub const CONSOLE: &str = "\u{EAE6}";
    pub const TRASH: &str = "\u{E4A6}";
    pub const SAVE: &str = "\u{E248}";
}

/// Embeds the icon font and puts it after the text font, so icons can be mixed into any label.
pub fn add_icon_font(fonts: &mut egui::FontDefinitions) {
    fonts.font_data.insert(
        "phosphor".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!("../assets/Phosphor.ttf"))),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let keys = fonts.families.entry(family).or_default();
        // after the text font: Phosphor also has (blank) Latin glyphs that must not win
        let at = keys.len().min(1);
        keys.insert(at, "phosphor".into());
    }
}

/// Colour tokens.
pub struct Tokens {
    /// window background (start screen)
    pub bg: Color32,
    /// bars and side panels
    pub surface: Color32,
    /// controls, menus, popovers
    pub raised: Color32,
    /// hovered or selected control
    pub hover: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub accent_text: Color32,
    pub danger: Color32,
    pub danger_soft: Color32,
    pub danger_text: Color32,
    pub success: Color32,
    pub success_soft: Color32,
    pub success_text: Color32,
    pub warning: Color32,
}

pub const DARK: Tokens = Tokens {
    bg: Color32::from_rgb(0x0E, 0x0F, 0x12),
    surface: Color32::from_rgb(0x16, 0x18, 0x1D),
    raised: Color32::from_rgb(0x1F, 0x22, 0x29),
    hover: Color32::from_rgb(0x2A, 0x2E, 0x37),
    border: Color32::from_rgb(0x2A, 0x2E, 0x37),
    text: Color32::from_rgb(0xE8, 0xEA, 0xED),
    muted: Color32::from_rgb(0x9A, 0xA0, 0xAB),
    faint: Color32::from_rgb(0x6B, 0x71, 0x7C),
    accent: Color32::from_rgb(0x2F, 0x6F, 0xE4),
    accent_soft: Color32::from_rgb(0x17, 0x23, 0x3A),
    accent_text: Color32::from_rgb(0xA9, 0xC4, 0xF8),
    danger: Color32::from_rgb(0xD9, 0x3A, 0x40),
    danger_soft: Color32::from_rgb(0x2A, 0x15, 0x17),
    danger_text: Color32::from_rgb(0xF0, 0x86, 0x8A),
    success: Color32::from_rgb(0x3D, 0xD6, 0x8C),
    success_soft: Color32::from_rgb(0x13, 0x3B, 0x29),
    success_text: Color32::from_rgb(0x7E, 0xE2, 0xB1),
    warning: Color32::from_rgb(0xE2, 0xA9, 0x3B),
};

pub const LIGHT: Tokens = Tokens {
    bg: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    surface: Color32::from_rgb(0xF6, 0xF7, 0xF9),
    raised: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    hover: Color32::from_rgb(0xEC, 0xEE, 0xF1),
    border: Color32::from_rgb(0xE3, 0xE5, 0xE8),
    text: Color32::from_rgb(0x14, 0x16, 0x1A),
    muted: Color32::from_rgb(0x5F, 0x66, 0x72),
    faint: Color32::from_rgb(0x8A, 0x90, 0x9B),
    accent: Color32::from_rgb(0x2F, 0x6F, 0xE4),
    accent_soft: Color32::from_rgb(0xE6, 0xEE, 0xFC),
    accent_text: Color32::from_rgb(0x1F, 0x55, 0xC0),
    danger: Color32::from_rgb(0xD9, 0x3A, 0x40),
    danger_soft: Color32::from_rgb(0xFD, 0xEC, 0xEC),
    danger_text: Color32::from_rgb(0xB4, 0x23, 0x28),
    success: Color32::from_rgb(0x1F, 0x9D, 0x63),
    success_soft: Color32::from_rgb(0xE3, 0xF6, 0xEC),
    success_text: Color32::from_rgb(0x16, 0x7A, 0x4C),
    warning: Color32::from_rgb(0xB7, 0x79, 0x1F),
};

/// Sizes in points.
pub mod size {
    /// controls in the bar and in popovers
    pub const CONTROL: f32 = 28.0;
    /// buttons and fields in forms (start screen)
    pub const FORM: f32 = 32.0;
    pub const RADIUS: u8 = 6;
    pub const PANEL_RADIUS: u8 = 10;
    pub const CAPTION: f32 = 12.0;
    pub const BODY: f32 = 13.0;
    pub const TITLE: f32 = 15.0;
    pub const HEADING: f32 = 20.0;
}

pub fn tokens(ui: &Ui) -> &'static Tokens {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

fn visuals(t: &Tokens, dark: bool) -> Visuals {
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    let r = CornerRadius::same(size::RADIUS);
    v.panel_fill = t.bg;
    v.window_fill = t.raised;
    v.window_stroke = Stroke::new(1.0, t.border);
    v.window_corner_radius = CornerRadius::same(size::PANEL_RADIUS);
    v.menu_corner_radius = CornerRadius::same(size::PANEL_RADIUS);
    v.extreme_bg_color = t.surface; // text fields, slider rails
    v.text_edit_bg_color = Some(t.surface);
    v.faint_bg_color = t.surface;
    v.code_bg_color = t.surface;
    v.hyperlink_color = t.accent_text;
    v.weak_text_color = Some(t.muted);
    v.warn_fg_color = t.warning;
    v.error_fg_color = t.danger_text;
    v.selection.bg_fill = t.accent;
    v.selection.stroke = Stroke::new(1.0, t.accent_text);
    v.slider_trailing_fill = true;
    v.widgets.noninteractive.bg_fill = t.surface;
    v.widgets.noninteractive.weak_bg_fill = t.surface;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, t.border); // separators, frames
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, t.text);
    v.widgets.noninteractive.corner_radius = r;
    for (w, fill, stroke) in [
        (&mut v.widgets.inactive, t.raised, t.border),
        (&mut v.widgets.hovered, t.hover, t.border),
        (&mut v.widgets.active, t.hover, t.accent),
        (&mut v.widgets.open, t.hover, t.border),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0, stroke);
        w.fg_stroke = Stroke::new(1.0, t.text);
        w.corner_radius = r;
        w.expansion = 0.0;
    }
    v
}

pub fn apply(ctx: &egui::Context) {
    for (theme, t, dark) in [(Theme::Light, &LIGHT, false), (Theme::Dark, &DARK, true)] {
        ctx.style_mut_of(theme, |s| {
            s.visuals = visuals(t, dark);
            s.spacing.item_spacing = egui::vec2(8.0, 8.0);
            s.spacing.button_padding = egui::vec2(10.0, 4.0);
            s.spacing.interact_size.y = size::CONTROL;
            s.spacing.menu_margin = Margin::same(6);
            for (style, px) in [
                (egui::TextStyle::Body, size::BODY),
                (egui::TextStyle::Button, size::BODY),
                (egui::TextStyle::Small, size::CAPTION),
                (egui::TextStyle::Heading, size::HEADING),
            ] {
                if let Some(f) = s.text_styles.get_mut(&style) {
                    f.size = px;
                }
            }
        });
    }
    ctx.set_theme(egui::ThemePreference::System);
}

// ---- text ----

pub fn muted(ui: &Ui, text: impl Into<String>) -> RichText {
    RichText::new(text).color(tokens(ui).muted)
}

pub fn caption(ui: &Ui, text: impl Into<String>) -> RichText {
    RichText::new(text).size(size::CAPTION).color(tokens(ui).muted)
}

/// Small upper-case label over a group of controls.
pub fn section_label(ui: &mut Ui, text: &str) {
    let c = tokens(ui).muted;
    ui.label(RichText::new(text.to_uppercase()).size(11.0).strong().color(c));
}

/// A tinted box for a problem the user can act on.
pub fn banner(ui: &mut Ui, title: Option<&str>, body: &str) {
    let t = tokens(ui);
    egui::Frame::new()
        .fill(t.danger_soft)
        .stroke(Stroke::new(1.0, t.danger.gamma_multiply(0.45)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(icon::WARNING).size(16.0).color(t.danger_text));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    if let Some(title) = title {
                        ui.label(RichText::new(title).strong().color(t.danger_text));
                    }
                    ui.add(egui::Label::new(RichText::new(body).color(t.text)).wrap());
                });
            });
        });
}

// ---- fields ----

/// A single-line text field as tall as the buttons beside it, its text centred vertically.
pub fn field(text: &mut String, height: f32) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(text)
        .min_size(egui::vec2(0.0, height))
        .align(egui::Align2::LEFT_CENTER)
        .margin(Margin::symmetric(8, 0))
}

// ---- buttons ----

fn sized(b: egui::Button<'_>, height: f32) -> egui::Button<'_> {
    b.min_size(egui::vec2(0.0, height)).corner_radius(CornerRadius::same(size::RADIUS))
}

/// The one main action of a screen or group.
pub fn primary(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    let t = tokens(ui);
    let b = egui::Button::new(RichText::new(text).color(Color32::WHITE).strong()).fill(t.accent).stroke(Stroke::NONE);
    ui.add_enabled(enabled, sized(b, size::FORM))
}

pub fn secondary(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    ui.add_enabled(enabled, sized(egui::Button::new(text), size::FORM))
}

/// A button without a frame until hovered.
pub fn ghost(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    ui.add_enabled(enabled, sized(egui::Button::new(text).frame_when_inactive(false), size::FORM))
}

pub fn destructive(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    let t = tokens(ui);
    let b = egui::Button::new(RichText::new(text).color(Color32::WHITE).strong()).fill(t.danger).stroke(Stroke::NONE);
    ui.add_enabled(enabled, sized(b, size::FORM))
}

/// A square icon-only button for the bar; the tooltip names it.
pub fn icon_button(ui: &mut Ui, glyph: &str, tooltip: &str) -> Response {
    let b = egui::Button::new(RichText::new(glyph).size(16.0))
        .frame_when_inactive(false)
        .min_size(egui::vec2(size::CONTROL, size::CONTROL))
        .corner_radius(CornerRadius::same(size::RADIUS));
    ui.add(b).on_hover_text(tooltip)
}

/// Icon and label, highlighted while what it opens is open.
pub fn toggle_button(ui: &mut Ui, glyph: &str, text: &str, on: bool) -> Response {
    let t = tokens(ui);
    let color = if on { t.accent_text } else { t.text };
    let mut b = egui::Button::new(RichText::new(format!("{glyph}  {text}")).color(color))
        .frame_when_inactive(on)
        .min_size(egui::vec2(0.0, size::CONTROL))
        .corner_radius(CornerRadius::same(size::RADIUS));
    if on {
        b = b.fill(t.accent_soft).stroke(Stroke::NONE);
    }
    ui.add(b)
}

/// One of a few options, as joined buttons. True when the value changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let t = tokens(ui);
    let before = *value;
    let n = options.len();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (i, (v, label)) in options.iter().enumerate() {
            let on = *value == *v;
            let r = size::RADIUS;
            let radius = CornerRadius {
                nw: if i == 0 { r } else { 0 },
                sw: if i == 0 { r } else { 0 },
                ne: if i + 1 == n { r } else { 0 },
                se: if i + 1 == n { r } else { 0 },
            };
            let text = RichText::new(*label).size(size::CAPTION).color(if on { t.text } else { t.muted });
            let b = egui::Button::new(text)
                .fill(if on { t.hover } else { t.surface })
                .stroke(Stroke::new(1.0, t.border))
                .corner_radius(radius)
                .min_size(egui::vec2(0.0, size::CONTROL));
            if ui.add(b).clicked() {
                *value = *v;
            }
        }
    });
    *value != before
}

// ---- small pieces ----

pub enum Tone {
    Neutral,
    Success,
}

pub fn badge(ui: &mut Ui, text: &str, tone: Tone) {
    let t = tokens(ui);
    let (fill, fg, dot) = match tone {
        Tone::Neutral => (t.hover, t.muted, None),
        Tone::Success => (t.success_soft, t.success_text, Some(t.success)),
    };
    egui::Frame::new().fill(fill).corner_radius(CornerRadius::same(10)).inner_margin(Margin::symmetric(8, 2)).show(
        ui,
        |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if let Some(c) = dot {
                    status_dot(ui, c);
                }
                ui.label(RichText::new(text).size(11.0).color(fg));
            });
        },
    );
}

pub fn status_dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

/// A thin vertical divider between groups in a bar.
pub fn vsep(ui: &mut Ui) {
    let t = tokens(ui);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 20.0), egui::Sense::hover());
    ui.painter().vline(rect.center().x, rect.y_range(), Stroke::new(1.0, t.border));
}

/// A bordered panel with a title, used for the groups of the start screen.
pub fn card<R>(ui: &mut Ui, title: &str, description: Option<&str>, body: impl FnOnce(&mut Ui) -> R) -> R {
    let t = tokens(ui);
    egui::Frame::new()
        .fill(t.surface)
        .stroke(Stroke::new(1.0, t.border))
        .corner_radius(CornerRadius::same(size::PANEL_RADIUS))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).strong().size(size::TITLE));
            if let Some(d) = description {
                ui.label(muted(ui, d));
            }
            ui.add_space(4.0);
            body(ui)
        })
        .inner
}
