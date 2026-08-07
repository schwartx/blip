//! blip — a cursor-anchored, draggable, always-on-top notification panel.
//!
//! Two binaries share this crate:
//!   * `blip`  — the CLI client (console subsystem, talks over a named pipe)
//!   * `blipd` — the resident daemon (windows subsystem, owns the panel)

pub mod config;
pub mod ipc;
pub mod model;
pub mod store;

#[cfg(windows)]
pub mod ui;

/// NUL-terminated UTF-16, ready to hand to a `*W` Win32 entry point.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16 without the terminator, for DirectWrite (which takes an explicit length).
pub fn wide_raw(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
