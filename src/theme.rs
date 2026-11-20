//! Omarchy swaps the current theme symlink on theme changes. Reopening the
//! small palette file picks up both atomic replacements and in-place edits.
use egui::Color32;
use std::{
    path::PathBuf,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

static CACHE: LazyLock<Mutex<Option<(Instant, Color32)>>> = LazyLock::new(Mutex::default);

fn read_accent() -> Option<Color32> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    let palette = std::fs::read_to_string(state.join("omarchy/current/theme/colors.toml")).ok()?;
    let (_, value) = palette
        .lines()
        .filter_map(|s| s.split_once('='))
        .find(|(key, _)| key.trim() == "accent")?;
    Color32::from_hex(value.trim().trim_matches('"')).ok()
}

pub fn accent() -> Color32 {
    let mut cached = CACHE.lock().unwrap();
    if let Some((when, value)) = *cached
        && when.elapsed() < Duration::from_secs(1)
    {
        return value;
    }
    let value = read_accent().unwrap_or(Color32::from_rgb(0xfa, 0xc5, 0x30));
    *cached = Some((Instant::now(), value));
    value
}

pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "Inter".into(),
        egui::FontData::from_static(include_bytes!("../assets/Inter.ttf")).into(),
    );
    fonts
        .families
        .get_mut(&egui::FontFamily::Proportional)
        .unwrap()
        .insert(0, "Inter".into());
    ctx.set_fonts(fonts);
}
