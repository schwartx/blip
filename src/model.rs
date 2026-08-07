//! Wire protocol + runtime notification model.
//!
//! All three transports (named pipe, HTTP, CLI) decode into `Command`, so the
//! daemon's policy engine only ever sees one type.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Doesn't pull the panel open — just lands in the list and bumps the tray badge.
    Low,
    #[default]
    Normal,
    /// Never expires on its own. Must be cleared by hand.
    Critical,
}

impl Level {
    /// Severity order, for picking the loudest level in a batch of arrivals.
    /// Not a `PartialOrd` impl: comparing two notification levels with `<` reads
    /// like it means something about time or count.
    pub fn rank(self) -> u8 {
        match self {
            Level::Low => 0,
            Level::Normal => 1,
            Level::Critical => 2,
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        match s.to_ascii_lowercase().as_str() {
            "low" | "l" => Some(Level::Low),
            "normal" | "n" | "info" => Some(Level::Normal),
            "critical" | "c" | "crit" | "error" | "err" => Some(Level::Critical),
            _ => None,
        }
    }
}

/// One incoming notification, exactly as it arrives on the wire.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NotifyRequest {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    /// Same id replaces the existing row in place instead of adding one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Free-form origin tag, used for grouping and per-source muting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Seconds before it fades out. `0` means sticky. Omitted = level default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<f32>,
    /// Shell command run when the row is clicked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound: Option<String>,
    /// 0-100. Presence marks this as an in-flight task, which suppresses the
    /// re-pop debounce so a progress stream doesn't keep yanking the panel open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Command {
    Notify(NotifyRequest),
    /// Withdraw a notification early (task cancelled, alert resolved).
    Dismiss { id: String },
    /// "Seen everything" — clear the list and hide.
    Clear,
    /// Force the panel open without adding anything.
    Show,
    /// Liveness probe used by the CLI to decide whether to spawn the daemon.
    Ping,
    /// Shut the daemon down cleanly, so the tray icon goes with it. Exists for
    /// installers and uninstallers, which otherwise have to `taskkill /F` and
    /// leave a ghost icon behind until the user happens to sweep the mouse
    /// across the tray. Unlike every other command, this must never cause a
    /// daemon to be spawned.
    Quit,
}

// ---------------------------------------------------------------------------
// Runtime model
// ---------------------------------------------------------------------------

/// A notification as the daemon holds it, with all the transient UI state.
#[derive(Clone, Debug)]
pub struct Notification {
    /// Monotonic internal handle. Stable across in-place updates, unlike `id`
    /// which is optional and caller-controlled.
    pub key: u64,
    pub id: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub level: Level,
    pub source: Option<String>,
    pub action: Option<String>,
    pub progress: Option<u8>,
    /// How many identical notifications collapsed into this row. Rendered as a
    /// `×N` badge rather than N separate rows.
    pub count: u32,

    /// Configured lifetime in seconds. `0.0` = never expires.
    pub ttl: f32,
    /// Counts down only while this row is actually on screen. See `store::tick`.
    pub remaining: f32,
    /// 1.0 right after an in-place update, decays to 0 — drives the flash so you
    /// can tell a row changed rather than silently swapping under you.
    pub flash: f32,
    /// Fade-in progress, 0..1.
    pub appear: f32,
    /// Set once expired or dismissed; the row fades out before being dropped.
    pub dying: bool,
    pub fade: f32,

    /// Cached measured height in DIP, invalidated when width or text changes.
    pub measured: Option<f32>,
}

impl Notification {
    pub fn is_sticky(&self) -> bool {
        self.ttl <= 0.0 || self.level == Level::Critical
    }
}
