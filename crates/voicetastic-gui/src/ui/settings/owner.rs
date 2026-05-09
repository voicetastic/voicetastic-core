//! User / Owner section.

use eframe::egui;

use super::widgets::str_field;
use super::{CardMeta, Ctx, card};
use crate::state::Section;

pub(super) fn section(ui: &mut egui::Ui, ctx: &Ctx<'_>) {
    card(
        ui,
        ctx,
        CardMeta {
            title: "👤 User / Owner",
            id: "owner",
            section: Section::Owner,
            name: "Owner",
        },
        |s| Some(s.owner.clone().unwrap_or_default()),
        |s, v| s.owner = Some(v),
        |ui, c| {
            let mut ch = false;
            ch |= str_field(ui, "Long name", &mut c.long_name);
            if str_field(ui, "Short name", &mut c.short_name) {
                // Cap by *characters*, not bytes — `truncate(4)` would panic
                // if it landed mid-codepoint on a multibyte character.
                if c.short_name.chars().count() > 4 {
                    c.short_name = c.short_name.chars().take(4).collect();
                }
                ch = true;
            }
            ch |= ui.checkbox(&mut c.is_licensed, "Licensed (HAM)").changed();
            ch
        },
        |svc, c| Box::pin(async move { svc.write_owner(c).await.map(|_| ()) }),
    );
}
