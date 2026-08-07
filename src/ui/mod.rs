//! The panel: window, renderer, layout, positioning, tray, audio.

pub mod audio;
pub mod autostart;
pub mod layout;
pub mod position;
pub mod render;
pub mod tray;
pub mod window;

use windows::Win32::UI::WindowsAndMessaging::WM_APP;

/// An IPC thread pushed something onto the queue.
pub const WM_APP_IPC: u32 = WM_APP + 1;
/// Tray icon callback.
pub const WM_APP_TRAY: u32 = WM_APP + 2;

/// Frame budget while animating. The panel is event-driven the rest of the
/// time — a tray-resident app that renders while nothing is happening is just a
/// battery drain.
pub const FRAME_MS: u32 = 16;
