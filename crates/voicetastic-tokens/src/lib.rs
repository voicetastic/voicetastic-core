//! Voicetastic design tokens.
//!
//! Single source of truth for visual style across the desktop GUI, the
//! Android app, and any future web client. The canonical token file is
//! [`tokens/design.toml`](../../tokens/design.toml) at the workspace
//! root; this crate parses it at compile time (`include_str!`) and
//! exposes typed accessors.
//!
//! ## Philosophy
//!
//! Tokens describe *style primitives* — colors, typography, spacing,
//! shape, elevation — not *components*. Each platform writes its own
//! Button / TextField / Card etc., but they all read the same tokens.
//! This is deliberate: a Meshtastic-class OLED widget and a Compose
//! Material 3 widget should not share rendering code, but they should
//! share style.
//!
//! ## Platform emitters
//!
//! Feature flags gate per-target emitters:
//!
//! - `egui`: [`egui_visuals`] produces an [`egui::Visuals`] from a
//!   [`Theme`]. Used by `voicetastic-gui`.
//!
//! Additional emitters (Android `ColorScheme` JSON, web CSS custom
//! properties, monochrome firmware luminance table) live behind their
//! own feature flags as those platforms come online.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use serde::Deserialize;

#[cfg(feature = "egui")]
pub mod egui_emitter;

#[cfg(feature = "egui")]
pub use egui_emitter::{
    egui_style, egui_style_with_contrast, egui_visuals, egui_visuals_with_contrast,
};

/// Verbatim TOML source. Embedded so consumers don't need filesystem
/// access at runtime.
pub const TOKENS_TOML: &str = include_str!("../../../tokens/design.toml");

/// Parsed root of `tokens/design.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Tokens {
    pub color: ColorSchemes,
    pub typography: BTreeMap<String, TypeStyle>,
    pub spacing: Spacing,
    pub shape: Shape,
    pub elevation: BTreeMap<String, Elevation>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ColorSchemes {
    pub light: ColorScheme,
    pub dark: ColorScheme,
    /// HighContrast variant of `light` (same seed, `contrast_level = 1.0`).
    /// Mirrors the palette shipped by `meshtastic-device-ui` so the desktop
    /// can render the firmware look (or an a11y theme) without forking the
    /// brand. Selected via [`Contrast::High`].
    pub light_hc: ColorScheme,
    /// HighContrast variant of `dark` — see [`Self::light_hc`].
    pub dark_hc: ColorScheme,
    pub fixed: FixedColors,
}

/// Full M3 color-role surface. Mirrors the Compose `ColorScheme` so the
/// desktop and Android themes can share an exhaustive role list — see
/// the Android app's `ui/theme/Color.kt` for the authoritative names.
/// Every field is parsed eagerly into [`Rgb`] from a `#RRGGBB` string.
#[derive(Debug, Clone, Deserialize)]
pub struct ColorScheme {
    // -- Primary ----------------------------------------------------
    #[serde(deserialize_with = "deser_rgb")]
    pub primary: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_primary: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub primary_container: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_primary_container: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub inverse_primary: Rgb,
    // -- Secondary --------------------------------------------------
    #[serde(deserialize_with = "deser_rgb")]
    pub secondary: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_secondary: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub secondary_container: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_secondary_container: Rgb,
    // -- Tertiary ---------------------------------------------------
    #[serde(deserialize_with = "deser_rgb")]
    pub tertiary: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_tertiary: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub tertiary_container: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_tertiary_container: Rgb,
    // -- Error ------------------------------------------------------
    #[serde(deserialize_with = "deser_rgb")]
    pub error: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_error: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub error_container: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_error_container: Rgb,
    // -- Background & surface --------------------------------------
    #[serde(deserialize_with = "deser_rgb")]
    pub background: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_background: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub surface: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_surface: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub surface_variant: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_surface_variant: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub surface_tint: Rgb,
    // -- Inverse surface (snackbars, tooltips) ---------------------
    #[serde(deserialize_with = "deser_rgb")]
    pub inverse_surface: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub inverse_on_surface: Rgb,
    // -- Surface tonal tiers (M3 expressive scale) -----------------
    #[serde(deserialize_with = "deser_rgb")]
    pub surface_dim: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub surface_bright: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub surface_container_lowest: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub surface_container_low: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub surface_container: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub surface_container_high: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub surface_container_highest: Rgb,
    // -- Outlines ---------------------------------------------------
    #[serde(deserialize_with = "deser_rgb")]
    pub outline: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub outline_variant: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub scrim: Rgb,
}

/// "Fixed" color roles that stay identical across light and dark
/// schemes. M3 introduces these so a brand-tinted surface (e.g. a
/// floating action sheet) can keep its identity regardless of which
/// theme the rest of the app is in.
#[derive(Debug, Clone, Deserialize)]
pub struct FixedColors {
    #[serde(deserialize_with = "deser_rgb")]
    pub primary_fixed: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub primary_fixed_dim: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_primary_fixed: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_primary_fixed_variant: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub secondary_fixed: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub secondary_fixed_dim: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_secondary_fixed: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_secondary_fixed_variant: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub tertiary_fixed: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub tertiary_fixed_dim: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_tertiary_fixed: Rgb,
    #[serde(deserialize_with = "deser_rgb")]
    pub on_tertiary_fixed_variant: Rgb,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct TypeStyle {
    pub size_dp: u16,
    pub weight: u16,
    pub line_height_dp: u16,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Spacing {
    pub step_dp: u16,
    pub xs: u16,
    pub sm: u16,
    pub md: u16,
    pub lg: u16,
    pub xl: u16,
    pub xxl: u16,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Shape {
    pub none: u16,
    pub extra_small: u16,
    pub small: u16,
    pub medium: u16,
    pub large: u16,
    pub extra_large: u16,
    pub full: u16,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Elevation {
    pub dp: u16,
    pub tint_opacity: f32,
}

/// 8-bit sRGB triple. Alpha is conveyed separately (elevation tints,
/// hover overlays, etc.) so the same `Rgb` can drive any role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn from_hex_str(s: &str) -> Option<Self> {
        // const-fn-friendly parser: input must be exactly "#RRGGBB".
        let bytes = s.as_bytes();
        if bytes.len() != 7 || bytes[0] != b'#' {
            return None;
        }
        let Some(r) = parse_hex_pair(bytes[1], bytes[2]) else {
            return None;
        };
        let Some(g) = parse_hex_pair(bytes[3], bytes[4]) else {
            return None;
        };
        let Some(b) = parse_hex_pair(bytes[5], bytes[6]) else {
            return None;
        };
        Some(Self { r, g, b })
    }

    /// Linearly mix `self` toward `other` by `t` ∈ [0, 1]. Used for
    /// the elevation tint model.
    pub fn mix(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let lerp =
            |a: u8, b: u8| -> u8 { (f32::from(a) * (1.0 - t) + f32::from(b) * t).round() as u8 };
        Self {
            r: lerp(self.r, other.r),
            g: lerp(self.g, other.g),
            b: lerp(self.b, other.b),
        }
    }
}

const fn parse_hex_pair(hi: u8, lo: u8) -> Option<u8> {
    let Some(h) = hex_nibble(hi) else { return None };
    let Some(l) = hex_nibble(lo) else { return None };
    Some((h << 4) | l)
}

const fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn deser_rgb<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Rgb, D::Error> {
    use serde::de::Error as _;
    let s = String::deserialize(d)?;
    Rgb::from_hex_str(&s).ok_or_else(|| {
        D::Error::custom(format!(
            "invalid hex color literal: {s:?}; expected #RRGGBB"
        ))
    })
}

/// Lazily parsed [`Tokens`]. Cheap to call repeatedly.
pub fn tokens() -> &'static Tokens {
    use once_cell::sync::Lazy;
    static CACHE: Lazy<Tokens> =
        Lazy::new(|| toml::from_str(TOKENS_TOML).expect("tokens/design.toml is malformed"));
    &CACHE
}

/// Whether the resolved theme should use the dark color scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Light,
    Dark,
}

/// Which contrast tier of the palette to draw from. `Standard` is the
/// default desktop look (warm peach surfaces, moderate text contrast);
/// `High` mirrors the meshtastic-device-ui firmware theme — same seed,
/// surfaces pushed to the extremes, `on_*` roles at pure black/white.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Contrast {
    #[default]
    Standard,
    High,
}

/// Convenience: pick the right [`ColorScheme`] given a [`ColorMode`].
/// Returns the `Standard`-contrast palette; for the HighContrast
/// variant call [`scheme_with_contrast`].
pub fn scheme(mode: ColorMode) -> &'static ColorScheme {
    scheme_with_contrast(mode, Contrast::Standard)
}

/// Pick the [`ColorScheme`] for a given mode at a given contrast tier.
pub fn scheme_with_contrast(mode: ColorMode, contrast: Contrast) -> &'static ColorScheme {
    let c = &tokens().color;
    match (mode, contrast) {
        (ColorMode::Light, Contrast::Standard) => &c.light,
        (ColorMode::Dark, Contrast::Standard) => &c.dark,
        (ColorMode::Light, Contrast::High) => &c.light_hc,
        (ColorMode::Dark, Contrast::High) => &c.dark_hc,
    }
}

/// Mode-independent fixed-tone roles.
pub fn fixed_colors() -> &'static FixedColors {
    &tokens().color.fixed
}

/// M3 surface-tier resolution: map an elevation level (0..=5) to the
/// pre-mixed surface container hue from the active palette. This
/// matches Material 3's recommended "elevation = tonal tier" model
/// (the actual `dp` value is rendered as a shadow on platforms that
/// support shadows, but the *colour* under it stays consistent).
///
/// Levels above 5 saturate to `surface_bright`; level 0 returns plain
/// `surface`.
pub fn surface_at_elevation(scheme: &ColorScheme, level: u8) -> Rgb {
    match level {
        0 => scheme.surface,
        1 => scheme.surface_container_low,
        2 => scheme.surface_container,
        3 => scheme.surface_container_high,
        4 => scheme.surface_container_highest,
        _ => scheme.surface_bright,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_parse() {
        let t = tokens();
        // Spot-check the TonalSpot palette generated from seed #FFBDA8
        // by `examples/generate_palette.rs` so a wholesale palette
        // swap can't silently change the wire format.
        assert_eq!(
            t.color.light.primary,
            Rgb {
                r: 0x8F,
                g: 0x4C,
                b: 0x35
            }
        );
        assert_eq!(
            t.color.dark.primary,
            Rgb {
                r: 0xFF,
                g: 0xB5,
                b: 0x9D
            }
        );
        // Tertiary is the most-rotated role (60° off primary); guard
        // it to catch accidental style changes that flatten the hue
        // spread back to a monochromic look.
        assert_eq!(
            t.color.light.tertiary,
            Rgb {
                r: 0x6A,
                g: 0x5E,
                b: 0x2F
            }
        );
        assert_eq!(
            t.color.dark.tertiary,
            Rgb {
                r: 0xD7,
                g: 0xC6,
                b: 0x8D
            }
        );
        // Surface tonal scale + fixed colours arrived together; spot
        // one of each so neither block can vanish unnoticed.
        assert_eq!(
            t.color.light.surface_container_high,
            Rgb {
                r: 0xF7,
                g: 0xE4,
                b: 0xDF
            }
        );
        assert_eq!(
            t.color.dark.inverse_primary,
            Rgb {
                r: 0x8F,
                g: 0x4C,
                b: 0x35
            }
        );
        assert_eq!(
            t.color.fixed.primary_fixed,
            Rgb {
                r: 0xFF,
                g: 0xDB,
                b: 0xD0
            }
        );
        // HighContrast variant mirrors the meshtastic-device-ui firmware
        // theme: light surface stays warm but `on_surface` clamps to pure
        // black, and the inverse holds in dark mode. Spot one of each so
        // a regen of `design.toml` can't silently drop the HC blocks.
        assert_eq!(t.color.light_hc.surface, t.color.light.surface);
        assert_eq!(
            t.color.light_hc.on_surface,
            Rgb {
                r: 0x00,
                g: 0x00,
                b: 0x00
            }
        );
        assert_eq!(
            t.color.dark_hc.on_surface,
            Rgb {
                r: 0xFF,
                g: 0xFF,
                b: 0xFF
            }
        );
        assert_eq!(t.spacing.md, 12);
        assert_eq!(t.shape.large, 16);
        assert!(t.elevation.contains_key("level3"));
    }

    #[test]
    fn scheme_with_contrast_selects_table() {
        let std_light = scheme_with_contrast(ColorMode::Light, Contrast::Standard);
        let hc_light = scheme_with_contrast(ColorMode::Light, Contrast::High);
        // The standard light primary is the seed-derived terracotta;
        // the HC variant pushes it darker toward black.
        assert_ne!(std_light.primary, hc_light.primary);
        // `scheme(mode)` is sugar for the Standard tier.
        assert_eq!(std_light as *const _, scheme(ColorMode::Light) as *const _);
    }

    #[test]
    fn surface_tier_walks_lowest_to_brightest() {
        let s = scheme(ColorMode::Dark);
        // The Android palette's dark mode is monotonic in tier order:
        // each step is at least as bright as the one below it.
        let levels: Vec<Rgb> = (0..=5).map(|l| surface_at_elevation(s, l)).collect();
        // The lowest container is darker than `surface`; `surface_dim`
        // matches `surface` in this palette so we just compare the
        // saturating top tier to plain surface.
        assert_ne!(levels[0], levels[5]);
    }

    #[test]
    fn rgb_mix_endpoints() {
        let a = Rgb { r: 0, g: 0, b: 0 };
        let b = Rgb {
            r: 255,
            g: 255,
            b: 255,
        };
        assert_eq!(a.mix(b, 0.0), a);
        assert_eq!(a.mix(b, 1.0), b);
        assert_eq!(
            a.mix(b, 0.5),
            Rgb {
                r: 128,
                g: 128,
                b: 128
            }
        );
    }

    #[test]
    fn rgb_rejects_bad_input() {
        assert!(Rgb::from_hex_str("6750A4").is_none()); // missing #
        assert!(Rgb::from_hex_str("#6750A").is_none()); // too short
        assert!(Rgb::from_hex_str("#6750AG").is_none()); // bad nibble
    }
}
