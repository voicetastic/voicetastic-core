//! Reusable input widgets and prost-enum bridging used by the settings tab.
//!
//! Nothing in this module knows about `SharedState`, the runtime, or the
//! mesh service — it's pure egui glue.

use eframe::egui;

// --------------------------------------------------------------------------
// Numeric / text fields
// --------------------------------------------------------------------------

fn parsed_field<T: std::str::FromStr + std::fmt::Display + PartialEq>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut T,
) -> bool {
    // Persist the in-flight buffer so transient unparseable states like
    // "1.", "-", or "1e" don't snap the field back to the last valid value
    // while the user is still typing.
    let id = ui.id().with(("parsed_field", label));
    let mut buf: String = ui
        .data_mut(|d| d.get_temp::<String>(id))
        .unwrap_or_else(|| value.to_string());
    // If the external value changed (device push) and the buffer no longer
    // parses to it, re-seed from the new value.
    if buf.parse::<T>().ok().as_ref() != Some(value) {
        buf = value.to_string();
    }
    let changed = ui
        .horizontal(|ui| {
            ui.label(label);
            ui.add(egui::TextEdit::singleline(&mut buf).desired_width(120.0))
                .changed()
        })
        .inner;
    let parsed = buf.parse::<T>().ok();
    ui.data_mut(|d| d.insert_temp(id, buf));
    if changed
        && let Some(n) = parsed
        && n != *value
    {
        *value = n;
        return true;
    }
    false
}

pub fn u32_field(ui: &mut egui::Ui, label: &str, value: &mut u32) -> bool {
    parsed_field(ui, label, value)
}

pub fn i32_field(ui: &mut egui::Ui, label: &str, value: &mut i32) -> bool {
    parsed_field(ui, label, value)
}

pub fn f32_field(ui: &mut egui::Ui, label: &str, value: &mut f32) -> bool {
    parsed_field(ui, label, value)
}

pub fn f64_field(ui: &mut egui::Ui, label: &str, value: &mut f64) -> bool {
    parsed_field(ui, label, value)
}

pub fn str_field(ui: &mut egui::Ui, label: &str, value: &mut String) -> bool {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.text_edit_singleline(value).changed()
    })
    .inner
}

/// Text field with a hide/show toggle. `salt` keeps the visibility state
/// distinct across multiple secret fields on the same screen.
pub fn secret_field(ui: &mut egui::Ui, label: &str, value: &mut String, salt: &str) -> bool {
    let id = egui::Id::new(salt);
    let mut visible = ui.data_mut(|d| *d.get_temp_mut_or(id, false));
    let changed = ui
        .horizontal(|ui| {
            ui.label(label);
            let resp = ui.add(egui::TextEdit::singleline(value).password(!visible));
            if ui.small_button(if visible { "🙈" } else { "👁" }).clicked() {
                visible = !visible;
            }
            resp.changed()
        })
        .inner;
    ui.data_mut(|d| d.insert_temp(id, visible));
    changed
}

// --------------------------------------------------------------------------
// Prost enum dropdown
// --------------------------------------------------------------------------

/// Bridge between prost enums (which expose `as_str_name` as an inherent
/// method, not a trait) and our generic combo helper.
pub trait EnumStrings: Sized + Copy + 'static {
    fn as_str_name_dyn(&self) -> &'static str;
    fn all_variants() -> Vec<(&'static str, i32)>;
}

#[macro_export]
macro_rules! impl_enum_strings {
    ($t:ty, [ $( $variant:ident ),* $(,)? ]) => {
        impl $crate::ui::settings::EnumStrings for $t {
            fn as_str_name_dyn(&self) -> &'static str { self.as_str_name() }
            fn all_variants() -> Vec<(&'static str, i32)> {
                #[allow(deprecated)]
                let v = vec![ $( (<$t>::$variant.as_str_name(), <$t>::$variant as i32) ),* ];
                v
            }
        }
    };
}

/// Render a dropdown over all known variants of a prost-generated enum.
/// `value` is the raw `i32` field on the proto; we render the textual name
/// so unknown firmware values round-trip safely (they'll display as `#N`).
pub fn enum_combo<E>(ui: &mut egui::Ui, label: &str, value: &mut i32, id_salt: &str) -> bool
where
    E: TryFrom<i32> + EnumStrings,
{
    let current = E::try_from(*value)
        .map(|e| e.as_str_name_dyn().to_string())
        .unwrap_or_else(|_| format!("#{value}"));
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        egui::ComboBox::from_id_salt(id_salt)
            .selected_text(&current)
            .show_ui(ui, |ui| {
                for (name, num) in E::all_variants() {
                    let mut sel = *value == num;
                    if ui.selectable_value(&mut sel, true, name).clicked() && *value != num {
                        *value = num;
                        changed = true;
                    }
                }
            });
    });
    changed
}

// --------------------------------------------------------------------------
// Hex <-> bytes
// --------------------------------------------------------------------------

pub fn bytes_to_hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        let _ = write!(&mut s, "{byte:02x}");
    }
    s
}

pub fn hex_to_bytes(hex: &str) -> Option<Vec<u8>> {
    let clean: String = hex
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .replace("0x", "");
    if !clean.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(clean.len() / 2);
    for chunk in clean.as_bytes().chunks(2) {
        let s = std::str::from_utf8(chunk).ok()?;
        out.push(u8::from_str_radix(s, 16).ok()?);
    }
    Some(out)
}
