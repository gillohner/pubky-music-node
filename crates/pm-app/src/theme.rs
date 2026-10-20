//! The app's look: one calm dark palette with a single accent, a clear type
//! scale, rounded surfaces, and a few custom widgets (nav items, play button,
//! pills, rounded covers) that egui's defaults do not offer. Mirrors the web
//! UI's design tokens so both clients feel like one product.

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Vec2};

pub const BG: Color32 = Color32::from_rgb(0x0f, 0x10, 0x14);
pub const SURFACE: Color32 = Color32::from_rgb(0x16, 0x17, 0x1d);
pub const SURFACE_2: Color32 = Color32::from_rgb(0x1d, 0x1f, 0x27);
pub const SURFACE_3: Color32 = Color32::from_rgb(0x26, 0x29, 0x35);
pub const LINE: Color32 = Color32::from_rgb(0x2a, 0x2c, 0x36);
pub const FG: Color32 = Color32::from_rgb(0xec, 0xed, 0xf2);
pub const FG_2: Color32 = Color32::from_rgb(0xc3, 0xc6, 0xd1);
pub const MUTED: Color32 = Color32::from_rgb(0x8d, 0x91, 0xa0);
pub const ACCENT: Color32 = Color32::from_rgb(0x8b, 0x7c, 0xf6);
pub const ACCENT_STRONG: Color32 = Color32::from_rgb(0xa4, 0x96, 0xff);
pub const ACCENT_SOFT: Color32 = Color32::from_rgba_premultiplied(0x2b, 0x27, 0x4d, 0xff);
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x0e, 0x0b, 0x1f);
pub const OK: Color32 = Color32::from_rgb(0x4f, 0xcf, 0x7f);
pub const WARN: Color32 = Color32::from_rgb(0xe8, 0xb2, 0x4a);
pub const BAD: Color32 = Color32::from_rgb(0xef, 0x6b, 0x6b);
pub const HEART: Color32 = Color32::from_rgb(0xff, 0x6b, 0x8a);

pub const Q_BIT_PERFECT: Color32 = ACCENT_STRONG;
pub const Q_ENHANCED: Color32 = Color32::from_rgb(0x5a, 0xa9, 0xff);
pub const Q_LOSSLESS: Color32 = OK;
pub const Q_LOSSY: Color32 = WARN;

pub const RADIUS: u8 = 8;

/// Install the theme on the context (dark only; the app has one look).
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.text_styles = [
        (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(22.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
    ]
    .into();
    style.spacing.item_spacing = Vec2::new(10.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    style.spacing.interact_size = Vec2::new(32.0, 28.0);
    style.spacing.menu_margin = egui::Margin::same(8);
    style.spacing.window_margin = egui::Margin::same(16);
    style.spacing.slider_width = 160.0;
    style.spacing.scroll.bar_width = 8.0;

    let v = &mut style.visuals;
    v.dark_mode = true;
    v.override_text_color = Some(FG);
    v.panel_fill = BG;
    v.window_fill = SURFACE_2;
    v.window_stroke = Stroke::new(1.0, LINE);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(10);
    v.extreme_bg_color = SURFACE;
    v.faint_bg_color = SURFACE;
    v.code_bg_color = SURFACE_3;
    v.hyperlink_color = ACCENT_STRONG;
    v.slider_trailing_fill = true;
    v.striped = true;
    v.selection.bg_fill = ACCENT_SOFT;
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.widgets.noninteractive.bg_fill = SURFACE;
    v.widgets.noninteractive.weak_bg_fill = SURFACE;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, FG_2);
    v.widgets.noninteractive.corner_radius = CornerRadius::same(RADIUS);
    v.widgets.inactive.bg_fill = SURFACE_2;
    v.widgets.inactive.weak_bg_fill = SURFACE_2;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, FG);
    v.widgets.inactive.corner_radius = CornerRadius::same(RADIUS);
    v.widgets.hovered.bg_fill = SURFACE_3;
    v.widgets.hovered.weak_bg_fill = SURFACE_3;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x3a, 0x3d, 0x4a));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, FG);
    v.widgets.hovered.corner_radius = CornerRadius::same(RADIUS);
    v.widgets.hovered.expansion = 0.0;
    v.widgets.active.bg_fill = ACCENT_SOFT;
    v.widgets.active.weak_bg_fill = ACCENT_SOFT;
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.active.fg_stroke = Stroke::new(1.0, FG);
    v.widgets.active.corner_radius = CornerRadius::same(RADIUS);
    v.widgets.open.bg_fill = SURFACE_3;
    v.widgets.open.weak_bg_fill = SURFACE_3;
    v.widgets.open.corner_radius = CornerRadius::same(RADIUS);
    v.window_shadow = egui::epaint::Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    v.popup_shadow = v.window_shadow;
    ctx.set_style_of(egui::Theme::Dark, style);
}

pub fn quality_color(q: pm_model::Quality) -> Color32 {
    match q {
        pm_model::Quality::BitPerfect => Q_BIT_PERFECT,
        pm_model::Quality::Enhanced => Q_ENHANCED,
        pm_model::Quality::Lossless => Q_LOSSLESS,
        pm_model::Quality::Lossy => Q_LOSSY,
    }
}

pub fn quality_label(q: pm_model::Quality) -> &'static str {
    match q {
        pm_model::Quality::BitPerfect => "Bit-perfect",
        pm_model::Quality::Enhanced => "Enhanced",
        pm_model::Quality::Lossless => "Lossless",
        pm_model::Quality::Lossy => "Lossy",
    }
}

/// Small uppercase section label (nav groups, eyebrows).
pub fn eyebrow(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .size(11.0)
            .strong()
            .color(MUTED),
    );
}

/// A navigation row: full width, rounded, accent-tinted when selected.
pub fn nav_item(ui: &mut egui::Ui, selected: bool, label: &str) -> egui::Response {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 32.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if selected {
            ACCENT_SOFT
        } else if resp.hovered() {
            SURFACE_2
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, CornerRadius::same(RADIUS), fill);
        let color = if selected { ACCENT_STRONG } else { FG_2 };
        ui.painter().text(
            egui::pos2(rect.min.x + 12.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            FontId::new(14.0, FontFamily::Proportional),
            color,
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A rounded pill with a coloured dot and text (quality, sync state).
pub fn pill(ui: &mut egui::Ui, dot: Option<Color32>, text: &str) -> egui::Response {
    let font = FontId::new(12.0, FontFamily::Proportional);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, FG_2);
    let dot_w = if dot.is_some() { 14.0 } else { 0.0 };
    let size = Vec2::new(galley.size().x + 22.0 + dot_w, 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::same(12), SURFACE_2);
        p.rect_stroke(
            rect,
            CornerRadius::same(12),
            Stroke::new(
                1.0,
                if resp.hovered() {
                    Color32::from_rgb(0x3a, 0x3d, 0x4a)
                } else {
                    LINE
                },
            ),
            egui::StrokeKind::Inside,
        );
        let mut x = rect.min.x + 11.0;
        if let Some(c) = dot {
            p.circle_filled(egui::pos2(x + 4.0, rect.center().y), 4.0, c);
            x += dot_w;
        }
        p.galley(
            egui::pos2(x, rect.center().y - galley.size().y / 2.0),
            galley,
            FG_2,
        );
    }
    resp
}

/// The round play/pause button of the transport bar.
pub fn play_button(ui: &mut egui::Ui, size: f32, playing: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let fill = if resp.hovered() { Color32::WHITE } else { FG };
        p.circle_filled(rect.center(), size / 2.0, fill);
        let c = rect.center();
        if playing {
            let w = size * 0.11;
            let h = size * 0.34;
            for dx in [-size * 0.12, size * 0.12] {
                p.rect_filled(
                    egui::Rect::from_center_size(egui::pos2(c.x + dx, c.y), Vec2::new(w, h)),
                    CornerRadius::same(1),
                    BG,
                );
            }
        } else {
            let s = size * 0.2;
            p.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(c.x - s * 0.8 + 1.0, c.y - s),
                    egui::pos2(c.x + s * 1.1 + 1.0, c.y),
                    egui::pos2(c.x - s * 0.8 + 1.0, c.y + s),
                ],
                BG,
                Stroke::NONE,
            ));
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A frameless round icon button (previous/next, row actions).
pub fn icon_button(ui: &mut egui::Ui, size: f32, glyph: &str, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if resp.hovered() {
            p.circle_filled(rect.center(), size / 2.0, SURFACE_3);
        }
        p.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            glyph,
            FontId::new(size * 0.5, FontFamily::Proportional),
            if resp.hovered() { FG } else { FG_2 },
        );
    }
    resp.on_hover_text(tip)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Cover art in `rect`: the texture with rounded corners, or a generated
/// placeholder from the title.
pub fn cover(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    tex: Option<&egui::TextureHandle>,
    title: &str,
    hovered: bool,
) {
    match tex {
        Some(t) => {
            let img = egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), rect.size()))
                .corner_radius(CornerRadius::same(RADIUS))
                .tint(if hovered {
                    Color32::from_white_alpha(235)
                } else {
                    Color32::WHITE
                });
            ui.put(rect, img);
        }
        None => placeholder(ui.painter(), rect, title, hovered),
    }
}

/// Placeholder cover: a gradient from the title and its initials.
pub fn placeholder(p: &egui::Painter, rect: egui::Rect, title: &str, hovered: bool) {
    let h = title
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    let hue = (h % 360) as f32 / 360.0;
    let (r, g, b) = hsl(hue, 0.38, if hovered { 0.36 } else { 0.30 });
    let (r2, g2, b2) = hsl((hue + 0.11) % 1.0, 0.42, 0.18);
    let top = Color32::from_rgb(r, g, b);
    let bottom = Color32::from_rgb(r2, g2, b2);
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    // Round the corners by painting the gradient then masking with the panel colour is
    // not possible in one pass; instead draw a rounded rect in the mid colour and the
    // gradient slightly inset, which reads as rounded at tile sizes.
    let mid = Color32::from_rgb(
        ((r as u16 + r2 as u16) / 2) as u8,
        ((g as u16 + g2 as u16) / 2) as u8,
        ((b as u16 + b2 as u16) / 2) as u8,
    );
    p.rect_filled(rect, CornerRadius::same(RADIUS), mid);
    let inset = rect.shrink(RADIUS as f32 * 0.6);
    let mut inner = egui::Mesh::default();
    inner.colored_vertex(inset.left_top(), top);
    inner.colored_vertex(inset.right_top(), top);
    inner.colored_vertex(inset.right_bottom(), bottom);
    inner.colored_vertex(inset.left_bottom(), bottom);
    inner.add_triangle(0, 1, 2);
    inner.add_triangle(0, 2, 3);
    p.add(egui::Shape::mesh(inner));
    let initials: String = title
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase();
    p.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initials,
        FontId::new((rect.height() * 0.26).max(10.0), FontFamily::Proportional),
        Color32::from_white_alpha(215),
    );
}

pub fn hsl(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h * 6.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

/// A primary (accent-filled) button.
pub fn primary(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.to_string()).color(ON_ACCENT).strong())
        .fill(ACCENT)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(14))
        .min_size(Vec2::new(0.0, 30.0))
}

/// A secondary (surface) button.
pub fn secondary(text: &str) -> egui::Button<'static> {
    egui::Button::new(text.to_string())
        .fill(SURFACE_2)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(14))
        .min_size(Vec2::new(0.0, 30.0))
}
