//! Configuration, loaded from `%APPDATA%\blip\config.toml`.
//!
//! Every field has a working default — the goal is that `blip "text"` with no
//! config file at all does the right thing.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::model::Level;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Listen address for the HTTP transport, which is always on.
    ///
    /// **There is no authentication.** The default accepts from anywhere, so
    /// that a Claude Code session or a build on another machine can reach the
    /// panel without editing a file first — which is most of the point of
    /// having an HTTP transport at all.
    ///
    /// The consequence is real: every host that can reach this port can put
    /// arbitrary content on a topmost window on your screen. Fine on a home
    /// network or behind Tailscale. On a network you don't control, set this
    /// to `127.0.0.1:7788`.
    pub bind: String,

    /// Hard cap on retained rows. Oldest are evicted first.
    pub max_items: usize,
    /// Panel grows to fit content up to this many rows, then scrolls.
    pub max_visible_rows: usize,

    /// Panel width in DIP.
    pub width: f32,

    pub font: String,
    pub font_size: f32,
    pub body_font_size: f32,

    /// Honour Windows' "don't interrupt me" states — presenting, full-screen
    /// D3D, Focus Assist busy.
    ///
    /// While one is active, `low` and `normal` notifications are collected
    /// silently instead of opening the panel, and the panel opens by itself
    /// once the state ends. `critical` still breaks through, on the grounds
    /// that an alert you only see three hours later isn't an alert.
    pub respect_quiet_hours: bool,

    pub sound: SoundConfig,
    pub levels: LevelConfig,
    pub theme: Theme,
    pub behavior: Behavior,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Behavior {
    /// Seconds the panel ignores mouse input right after appearing, so a click
    /// already in flight toward whatever was underneath doesn't hit us instead.
    pub input_grace: f32,
    /// Gap in DIP between the cursor and the panel's near corner. Must be large
    /// enough that the panel never covers the cursor hotspot itself.
    pub cursor_gap: f32,
    /// Once dragged, the panel stops following the cursor and stays put.
    pub drag_to_pin: bool,
    /// Same-id updates within this window won't re-open a hidden panel. Keeps a
    /// progress stream from yanking the panel open on every percent.
    pub repop_debounce: f32,
    /// Fade in / out duration in seconds.
    pub anim: f32,
    /// Release the GPU + audio devices after this many idle seconds. The process
    /// and its listeners stay up; the next notification pays a lazy rebuild.
    pub idle_release: f32,
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            input_grace: 0.35,
            cursor_gap: 18.0,
            drag_to_pin: true,
            repop_debounce: 1.5,
            anim: 0.18,
            idle_release: 90.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SoundConfig {
    pub enabled: bool,
    /// Absolute path to a WAV file, or empty for the built-in chime.
    pub low: String,
    pub normal: String,
    pub critical: String,
}

impl Default for SoundConfig {
    fn default() -> Self {
        Self { enabled: true, low: String::new(), normal: String::new(), critical: String::new() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LevelConfig {
    /// Default TTL in seconds per level. `0` = sticky.
    pub low_ttl: f32,
    pub normal_ttl: f32,
    pub critical_ttl: f32,
    /// Whether arrival of each level pulls the panel open.
    pub low_pops: bool,
    pub normal_pops: bool,
    pub critical_pops: bool,
}

impl Default for LevelConfig {
    fn default() -> Self {
        Self {
            low_ttl: 4.0,
            normal_ttl: 7.0,
            critical_ttl: 0.0,
            low_pops: false,
            normal_pops: true,
            critical_pops: true,
        }
    }
}

impl LevelConfig {
    pub fn ttl_for(&self, level: Level) -> f32 {
        match level {
            Level::Low => self.low_ttl,
            Level::Normal => self.normal_ttl,
            Level::Critical => self.critical_ttl,
        }
    }
    pub fn pops_for(&self, level: Level) -> bool {
        match level {
            Level::Low => self.low_pops,
            Level::Normal => self.normal_pops,
            Level::Critical => self.critical_pops,
        }
    }
}

/// Colors as `#RRGGBB` or `#RRGGBBAA`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    pub bg: String,
    pub border: String,
    pub title: String,
    pub body: String,
    pub row_hover: String,
    pub separator: String,
    pub button_bg: String,
    pub button_hover: String,
    pub button_text: String,
    pub scrollbar: String,
    pub flash: String,
    pub level_low: String,
    pub level_normal: String,
    pub level_critical: String,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            bg: "#16161AF2".into(),
            border: "#FFFFFF14".into(),
            title: "#F2F2F5".into(),
            body: "#F2F2F599".into(),
            row_hover: "#FFFFFF0F".into(),
            separator: "#FFFFFF0D".into(),
            button_bg: "#FFFFFF0A".into(),
            button_hover: "#FFFFFF1A".into(),
            button_text: "#F2F2F5CC".into(),
            scrollbar: "#FFFFFF26".into(),
            flash: "#4C8DFF".into(),
            level_low: "#6B7280".into(),
            level_normal: "#4C8DFF".into(),
            level_critical: "#FF5A52".into(),
        }
    }
}

impl Theme {
    pub fn level_color(&self, level: Level) -> &str {
        match level {
            Level::Low => &self.level_low,
            Level::Normal => &self.level_normal,
            Level::Critical => &self.level_critical,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:7788".into(),
            max_items: 50,
            max_visible_rows: 10,
            width: 340.0,
            // Yahei is on every Chinese Windows install and DirectWrite handles
            // fallback for anything it lacks.
            font: "Microsoft YaHei UI".into(),
            font_size: 13.5,
            body_font_size: 12.0,
            respect_quiet_hours: true,
            sound: SoundConfig::default(),
            levels: LevelConfig::default(),
            theme: Theme::default(),
            behavior: Behavior::default(),
        }
    }
}

impl Config {
    pub fn dir() -> PathBuf {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
        PathBuf::from(base).join("blip")
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.toml")
    }

    /// Never fails: a missing or malformed file falls back to defaults. A daemon
    /// that refuses to start because of a typo in a color is worse than one that
    /// starts with the wrong color.
    pub fn load() -> (Self, Option<String>) {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(cfg) => (cfg, None),
                Err(e) => (Config::default(), Some(format!("config parse error: {e}"))),
            },
            Err(_) => (Config::default(), None),
        }
    }

    /// Write a fully-populated config so the file itself documents the options.
    pub fn write_default() -> std::io::Result<PathBuf> {
        let dir = Self::dir();
        std::fs::create_dir_all(&dir)?;
        let path = Self::path();
        let text = toml::to_string_pretty(&Config::default())
            .unwrap_or_else(|e| format!("# serialize failed: {e}\n"));
        std::fs::write(&path, text)?;
        Ok(path)
    }
}

/// Parse `#RGB`, `#RRGGBB` or `#RRGGBBAA` into straight (non-premultiplied) RGBA.
pub fn parse_color(s: &str) -> (f32, f32, f32, f32) {
    let h = s.trim().trim_start_matches('#');
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0) as f32 / 255.0;
    match h.len() {
        3 => {
            let n = |i: usize| {
                u8::from_str_radix(&h[i..i + 1], 16).map(|v| (v * 17) as f32 / 255.0).unwrap_or(0.0)
            };
            (n(0), n(1), n(2), 1.0)
        }
        6 => (byte(0), byte(2), byte(4), 1.0),
        8 => (byte(0), byte(2), byte(4), byte(6)),
        _ => (1.0, 0.0, 1.0, 1.0), // magenta: loudly wrong beats silently black
    }
}
