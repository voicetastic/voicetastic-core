//! `voicetastic settings ...` — generic access to the centralised
//! [`SettingsApi`]. Output is plain text so it's easy to script.

use anyhow::{Context, Result};

use voicetastic_core::settings::{SettingKey, SettingKind, SettingsApi};

fn api() -> std::sync::Arc<SettingsApi> {
    SettingsApi::open()
}

fn parse_key(s: &str) -> Result<SettingKey> {
    SettingKey::from_id(s).with_context(|| format!("unknown setting `{s}`"))
}

pub fn list() -> Result<()> {
    let api = api();
    for d in api.list() {
        let kind = match &d.kind {
            SettingKind::OptionalString => "string?".to_string(),
            SettingKind::IntRange { min, max } => format!("int [{min}..={max}]"),
            SettingKind::Enum { variants } => format!("enum {{{}}}", variants.join("|")),
            SettingKind::Bool => "bool".to_string(),
        };
        let shown = if d.value.is_empty() {
            "<unset>".to_string()
        } else {
            d.value.clone()
        };
        println!(
            "{id:<32} {kind:<24} = {value}  (default: {default})",
            id = d.key.id(),
            value = shown,
            default = d.default,
        );
        println!("    {}", d.help);
    }
    Ok(())
}

pub fn get(key: &str) -> Result<()> {
    let api = api();
    let key = parse_key(key)?;
    print!("{}", api.get_str(key));
    Ok(())
}

pub fn set(key: &str, value: &str) -> Result<()> {
    let api = api();
    let key = parse_key(key)?;
    api.set_str(key, value)
        .with_context(|| format!("setting `{}` to `{value}`", key.id()))?;
    println!("set {} = {}", key.id(), api.get_str(key));
    Ok(())
}

pub fn reset(key: Option<&str>) -> Result<()> {
    let api = api();
    match key {
        Some(k) => {
            let key = parse_key(k)?;
            api.reset(key)
                .with_context(|| format!("resetting `{}`", key.id()))?;
            println!("reset {} -> {}", key.id(), api.get_str(key));
        }
        None => {
            api.reset_all().context("resetting all settings")?;
            println!("reset all settings to defaults");
        }
    }
    Ok(())
}
