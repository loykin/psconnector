//! The viewer's design system: tokens (colours, sizes, type), icons and the few components every screen is
//! built from. Screens use only these, so the look stays consistent and is the same on macOS and Windows.
//!
//! Follows the system light / dark setting; the streaming window is dark by nature, so dark is the
//! reference palette.
//!
//! The tokens (colours, sizes, icons) are defined once in `design/tokens.json` and generated into this module
//! by build.rs; the rules for using them are in `design/README.md`.

use eframe::egui::{self, Color32, CornerRadius, Margin, Response, RichText, Stroke, Theme, Ui, Visuals};

include!(concat!(env!("OUT_DIR"), "/tokens.rs"));

/// Embeds the icon font and puts it after the text font, so icons can be mixed into any label.
pub fn add_icon_font(fonts: &mut egui::FontDefinitions) {
    fonts.font_data.insert(
        "phosphor".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!("../../assets/Phosphor.ttf"))),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let keys = fonts.families.entry(family).or_default();
        // after the text font: Phosphor also has (blank) Latin glyphs that must not win
        let at = keys.len().min(1);
        keys.insert(at, "phosphor".into());
    }
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
            s.spacing.item_spacing = egui::vec2(size::GAP, size::GAP);
            s.spacing.button_padding = egui::vec2(10.0, 4.0);
            s.spacing.interact_size.y = size::CONTROL;
            s.spacing.menu_margin = Margin::same(6);
            // A thin bar at the very edge that does not widen on hover. Panels keep a gutter for it
            // (see `side_panel`), so it never covers sliders or buttons at the right edge.
            s.spacing.scroll = egui::style::ScrollStyle {
                bar_width: size::SCROLL_BAR,
                floating_width: size::SCROLL_BAR,
                floating_allocated_width: 0.0,
                handle_min_length: 24.0,
                bar_inner_margin: 0.0,
                bar_outer_margin: 0.0,
                // the handle stays faintly visible, so it is clear the panel scrolls
                dormant_background_opacity: 0.0,
                dormant_handle_opacity: 0.35,
                active_background_opacity: 0.25,
                active_handle_opacity: 0.6,
                interact_background_opacity: 0.4,
                interact_handle_opacity: 0.9,
                ..egui::style::ScrollStyle::floating()
            };
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
    ui.label(RichText::new(text.to_uppercase()).size(size::LABEL).strong().color(c));
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
    let b = egui::Button::new(RichText::new(glyph).size(size::ICON))
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
                ui.label(RichText::new(text).size(size::LABEL).color(fg));
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

/// A side panel: fixed title row with a close button, then content that scrolls when the window is short.
/// The panel's right padding holds the scroll bar, so the content and the title end at the same edge
/// whether or not it scrolls. Returns true when the close button was pressed.
pub fn side_panel(
    ui: &mut Ui,
    id: &str,
    width: f32,
    title: &str,
    description: Option<&str>,
    body: impl FnOnce(&mut Ui),
) -> bool {
    let t = tokens(ui);
    let pad = size::PANEL_PAD;
    let gutter = (size::SCROLL_BAR + size::SCROLL_GAP) as i8;
    let frame = egui::Frame::new().fill(t.surface).inner_margin(Margin {
        left: pad,
        right: pad - gutter,
        top: pad,
        bottom: pad,
    });
    let mut closed = false;
    egui::Panel::right(id.to_string()).exact_size(width).resizable(false).frame(frame).show(ui, |ui| {
        egui::Frame::new().inner_margin(Margin { right: gutter, ..Margin::ZERO }).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).strong().size(size::TITLE));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    closed = icon_button(ui, icon::CLOSE, "Close").clicked();
                });
            });
            if let Some(d) = description {
                ui.label(caption(ui, d));
            }
        });
        ui.add_space(8.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // the gutter is kept whether or not the bar shows, so the right edge never moves
            egui::Frame::new().inner_margin(Margin { right: gutter, ..Margin::ZERO }).show(ui, body);
        });
    });
    closed
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

#[cfg(test)]
mod tests {
    /// Web pages use design/tokens.css; it must be the CSS generated from design/tokens.json.
    /// `UPDATE_DESIGN_TOKENS=1 cargo test -p psconnector-view tokens` rewrites it.
    #[test]
    fn web_tokens_are_generated_from_the_same_file() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../design/tokens.css");
        if std::env::var_os("UPDATE_DESIGN_TOKENS").is_some() {
            std::fs::write(&path, super::TOKENS_CSS).unwrap();
        }
        let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            on_disk == super::TOKENS_CSS,
            "design/tokens.css is out of date: run UPDATE_DESIGN_TOKENS=1 cargo test -p psconnector-view tokens"
        );
    }
}
