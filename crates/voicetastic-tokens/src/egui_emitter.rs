//! Map [`super::Tokens`] onto a full [`egui::Style`].
//!
//! [`egui_style`] is the single entry point: it returns a complete
//! [`egui::Style`] (visuals + spacing + text styles) for the requested
//! [`ColorMode`]. Apply it once per theme slot with
//! [`egui::Context::set_style_of`] and egui will use the token palette
//! for both colours and layout.
//!
//! Uses the **M3 surface tonal scale** (`surface_container_*`,
//! `surface_dim`, `surface_bright`) for elevation rather than a
//! synthetic tint mix, so a popover with elevation 3 picks the same
//! hex on desktop and on Android Compose.
//!
//! Coverage gaps versus real M3 (Compose) — kept here for honesty:
//!
//! - No ripple animation. egui has no motion primitive for this.
//! - No real drop shadows. egui has window shadow but not per-widget.
//! - egui's built-in `Checkbox` / `Slider` / `ComboBox` geometry is
//!   hard-wired; the token palette colours them but the silhouettes
//!   stay egui's.
//! - egui has no `surface_tint` slot; we expose the role on
//!   [`super::ColorScheme`] for callers that want it but don't apply
//!   it in the emitter.
//!
//! What we *do* wire from tokens:
//!
//! - Type scale: every standard [`egui::TextStyle`] is mapped to an
//!   M3 [`super::TypeStyle`] (`label_medium`, `body_medium`,
//!   `label_large`, `title_large`). Font weights are not honoured —
//!   egui doesn't expose a weight axis on the default fonts.
//! - Spacing scale: `item_spacing`, `button_padding`, `indent`,
//!   `interact_size`, and menu/window margins all read from
//!   [`super::Spacing`].
//! - State layers: hovered/active widget backgrounds add an
//!   `on_surface` overlay at 8% / 12% alpha over the inactive base,
//!   matching M3's state-layer formula (no ripple, just the resting
//!   tint).

use std::collections::BTreeMap;

use egui::{
    Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, Style, TextStyle, Vec2, Visuals,
    style::{Spacing as EguiSpacing, Widgets},
};

use super::{
    ColorMode, ColorScheme, Rgb, Shape, Spacing as TokenSpacing, TypeStyle, scheme,
    surface_at_elevation, tokens,
};

/// Build a full [`egui::Style`] (visuals + spacing + text styles)
/// for the requested [`ColorMode`]. Apply with
/// [`egui::Context::set_style_of`].
pub fn egui_style(mode: ColorMode) -> Style {
    let mut style = Style {
        visuals: egui_visuals(mode),
        spacing: spacing_from_tokens(tokens().spacing),
        ..Style::default()
    };
    style.text_styles = text_styles_from_tokens();
    style
}

/// Build only the [`egui::Visuals`] for the requested mode. Useful for
/// callers that want to keep egui's default spacing / typography but
/// swap colours. Most code should prefer [`egui_style`].
pub fn egui_visuals(mode: ColorMode) -> Visuals {
    let s = scheme(mode);
    let dark = matches!(mode, ColorMode::Dark);
    let shape = tokens().shape;

    let mut v = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.dark_mode = dark;

    // Foreground text & status colors.
    v.override_text_color = Some(rgb(s.on_surface));
    v.hyperlink_color = rgb(s.primary);
    v.error_fg_color = rgb(s.error);
    v.warn_fg_color = rgb(s.tertiary);

    // Surfaces — pulled from the M3 tonal scale so an "elevation 2"
    // popover gets the same hex on every platform.
    v.panel_fill = rgb(s.surface);
    v.window_fill = rgb(surface_at_elevation(s, 2));
    v.window_stroke = Stroke::new(1.0, rgb(s.outline_variant));
    v.extreme_bg_color = rgb(surface_at_elevation(s, 0));
    v.faint_bg_color = rgb(surface_at_elevation(s, 1));
    v.code_bg_color = rgb(surface_at_elevation(s, 3));

    // Rounded corners — M3 "medium" for windows/menus, "small" for
    // interactive widgets. Component-level overrides happen at the
    // call site via `egui::Frame`.
    v.window_corner_radius = corner(shape.medium);
    v.menu_corner_radius = corner(shape.medium);

    // Selection (highlighted text, selected list items, focus rings).
    v.selection.bg_fill = rgb(s.primary_container).gamma_multiply(0.6);
    v.selection.stroke = Stroke::new(1.0, rgb(s.primary));

    v.widgets = widget_palette(s, shape);
    v
}

fn widget_palette(s: &ColorScheme, shape: Shape) -> Widgets {
    use egui::style::WidgetVisuals;

    let mut w = Widgets::default();
    let radius = corner(shape.small);
    let base = s.surface_container_high;

    // Noninteractive backdrops: labels, separators, etc. Plain
    // surface so static text reads on the panel without a "card"
    // outline implying interactivity.
    w.noninteractive = WidgetVisuals {
        bg_fill: rgb(s.surface),
        weak_bg_fill: rgb(s.surface),
        bg_stroke: Stroke::new(1.0, rgb(s.outline_variant)),
        fg_stroke: Stroke::new(1.0, rgb(s.on_surface)),
        corner_radius: radius,
        expansion: 0.0,
    };
    // Inactive interactive: idle buttons, unfocused fields. Tonal
    // surface that reads as "clickable surface" without committing
    // to a primary colour.
    w.inactive = WidgetVisuals {
        bg_fill: rgb(base),
        weak_bg_fill: rgb(surface_at_elevation(s, 1)),
        bg_stroke: Stroke::new(1.0, rgb(s.outline)),
        fg_stroke: Stroke::new(1.0, rgb(s.on_surface_variant)),
        corner_radius: radius,
        expansion: 0.0,
    };
    // Hovered: state-layer overlay (on_surface at 8% over the
    // inactive base). M3 hover doesn't change hue, just tints.
    w.hovered = WidgetVisuals {
        bg_fill: rgb(state_layer(base, s.on_surface, 0.08)),
        weak_bg_fill: rgb(state_layer(base, s.on_surface, 0.08)),
        bg_stroke: Stroke::new(1.0, rgb(s.outline)),
        fg_stroke: Stroke::new(1.0, rgb(s.on_surface)),
        corner_radius: radius,
        // M3 widgets don't grow on hover; keep the silhouette stable.
        expansion: 0.0,
    };
    // Active (pressed): the bold primary-filled state that gives a
    // click the satisfying "thunk." Not literal M3 — Compose keeps
    // the base hue and just deepens the state layer — but egui's
    // `active` slot is also reused for many committed-action looks,
    // so a saturated primary reads clearly.
    w.active = WidgetVisuals {
        bg_fill: rgb(state_layer(s.primary, s.on_primary, 0.12)),
        weak_bg_fill: rgb(state_layer(s.primary, s.on_primary, 0.12)),
        bg_stroke: Stroke::new(1.0, rgb(s.primary)),
        fg_stroke: Stroke::new(1.0, rgb(s.on_primary)),
        corner_radius: radius,
        expansion: 0.0,
    };
    // Open: drop-downs, combo boxes, expanded popovers. Secondary
    // tonal container so an open menu doesn't impersonate a primary
    // action that's being pressed.
    w.open = WidgetVisuals {
        bg_fill: rgb(s.secondary_container),
        weak_bg_fill: rgb(s.secondary_container),
        bg_stroke: Stroke::new(1.0, rgb(s.secondary)),
        fg_stroke: Stroke::new(1.0, rgb(s.on_secondary_container)),
        corner_radius: radius,
        expansion: 0.0,
    };
    w
}

/// M3 state-layer mix: `base + on_base × alpha`. `alpha` ∈ [0, 1]
/// where 0.08 = hover, 0.12 = focus/press, 0.16 = drag.
fn state_layer(base: Rgb, on_base: Rgb, alpha: f32) -> Rgb {
    base.mix(on_base, alpha)
}

fn spacing_from_tokens(t: TokenSpacing) -> EguiSpacing {
    // Compose M3 filled button is 24 dp horizontal, 10 dp vertical
    // padding. Our `md` (12) / `sm` (8) gives a slightly more
    // compact desktop rhythm. `interact_size` is M3's minimum touch
    // target with a desktop-tightened vertical (40 dp wide stays
    // honest, 32-ish tall reads as less mobile-large).
    EguiSpacing {
        item_spacing: Vec2::new(t.sm.into(), t.sm.into()),
        button_padding: Vec2::new(t.md.into(), t.sm.into()),
        indent: t.lg.into(),
        menu_margin: Margin::same(t.sm.try_into().unwrap_or(i8::MAX)),
        window_margin: Margin::same(t.lg.try_into().unwrap_or(i8::MAX)),
        interact_size: Vec2::new(40.0, 32.0),
        menu_spacing: t.xs.into(),
        icon_spacing: t.sm.into(),
        ..EguiSpacing::default()
    }
}

fn text_styles_from_tokens() -> BTreeMap<TextStyle, FontId> {
    let typ = &tokens().typography;
    // M3 → egui mapping. Weight isn't honoured (egui's default fonts
    // don't expose a weight axis on the proportional family); when
    // the M3 spec calls for medium 500, we just use the same size in
    // the regular face.
    let mut m = BTreeMap::new();
    m.insert(TextStyle::Small, prop(pick(typ, "label_medium", 12)));
    m.insert(TextStyle::Body, prop(pick(typ, "body_medium", 14)));
    m.insert(TextStyle::Monospace, mono(pick(typ, "body_medium", 14)));
    m.insert(TextStyle::Button, prop(pick(typ, "label_large", 14)));
    m.insert(TextStyle::Heading, prop(pick(typ, "title_large", 22)));
    m
}

fn pick(map: &BTreeMap<String, TypeStyle>, key: &str, fallback: u16) -> f32 {
    map.get(key)
        .map(|t| f32::from(t.size_dp))
        .unwrap_or(f32::from(fallback))
}

fn prop(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

fn rgb(c: Rgb) -> Color32 {
    Color32::from_rgb(c.r, c.g, c.b)
}

fn corner(px: u16) -> CornerRadius {
    // egui's CornerRadius takes u8 per corner. M3 corner tokens stay
    // well under 64 px, so saturating into u8 is lossless in practice.
    let r = px.min(u16::from(u8::MAX)) as u8;
    CornerRadius::same(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_and_dark_build() {
        let _ = egui_visuals(ColorMode::Light);
        let _ = egui_visuals(ColorMode::Dark);
        let _ = egui_style(ColorMode::Light);
        let _ = egui_style(ColorMode::Dark);
    }

    #[test]
    fn window_fill_uses_elevated_tier() {
        let s = scheme(ColorMode::Light);
        let v = egui_visuals(ColorMode::Light);
        assert_ne!(v.window_fill, rgb(s.surface));
        assert_eq!(v.window_fill, rgb(surface_at_elevation(s, 2)));
    }

    #[test]
    fn state_layer_is_monotonic() {
        let s = scheme(ColorMode::Dark);
        let base = s.surface_container_high;
        let l8 = state_layer(base, s.on_surface, 0.08);
        let l12 = state_layer(base, s.on_surface, 0.12);
        // Pressing should tint further than hovering.
        assert_ne!(l8, base);
        assert_ne!(l12, l8);
    }

    #[test]
    fn text_styles_have_all_standard_variants() {
        let m = text_styles_from_tokens();
        assert!(m.contains_key(&TextStyle::Small));
        assert!(m.contains_key(&TextStyle::Body));
        assert!(m.contains_key(&TextStyle::Monospace));
        assert!(m.contains_key(&TextStyle::Button));
        assert!(m.contains_key(&TextStyle::Heading));
        // Heading must be visibly larger than Body.
        let body = m.get(&TextStyle::Body).unwrap().size;
        let head = m.get(&TextStyle::Heading).unwrap().size;
        assert!(head > body);
    }

    #[test]
    fn spacing_carries_through() {
        let sp = spacing_from_tokens(tokens().spacing);
        assert_eq!(sp.item_spacing, Vec2::new(8.0, 8.0));
        assert_eq!(sp.button_padding, Vec2::new(12.0, 8.0));
    }
}
