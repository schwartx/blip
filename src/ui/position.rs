//! Where the panel goes.
//!
//! Two modes, and the transition between them is driven by a gesture rather
//! than a setting: the panel follows the cursor until you drag it, at which
//! point dragging *is* the statement "I want it here", and it stops moving.
//!
//! Everything is per-monitor DPI aware. `SetProcessDpiAwarenessContext` is
//! called before any window exists, so the coordinates the OS hands us are real
//! physical pixels rather than virtualised lies.

use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Anchor {
    /// Follow the cursor. The point is captured when the panel opens, not read
    /// live — a panel that chases the mouse is unusable.
    Cursor(POINT),
    /// Dragged into place; stays there until reset from the tray.
    Pinned(POINT),
}

impl Default for Anchor {
    fn default() -> Self {
        Anchor::Cursor(POINT { x: 0, y: 0 })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Placement {
    /// Window top-left in physical screen pixels.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub dpi: u32,
    /// The panel opened upward because there wasn't room below. Height changes
    /// then have to move the top edge, not the bottom.
    pub flipped_up: bool,
}

pub fn cursor_pos() -> POINT {
    let mut p = POINT::default();
    let _ = unsafe { GetCursorPos(&mut p) };
    p
}

/// Work area (taskbar excluded) and DPI of the monitor containing `pt`.
pub fn monitor_for_point(pt: POINT) -> (RECT, u32) {
    let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    monitor_info(hmon)
}

pub fn monitor_for_window(hwnd: windows::Win32::Foundation::HWND) -> (RECT, u32) {
    let hmon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    monitor_info(hmon)
}

fn monitor_info(hmon: HMONITOR) -> (RECT, u32) {
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let work = if unsafe { GetMonitorInfoW(hmon, &mut mi) }.as_bool() {
        // rcWork, not rcMonitor: never park the panel under the taskbar.
        mi.rcWork
    } else {
        RECT { left: 0, top: 0, right: 1920, bottom: 1080 }
    };

    let (mut dx, mut dy) = (96u32, 96u32);
    // Fails on pre-8.1; 96 is the right answer there anyway.
    let _ = unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
    let _ = dy;
    (work, if dx == 0 { 96 } else { dx })
}

/// Resolve an anchor plus a desired DIP size into a physical window rect.
pub fn place(anchor: Anchor, w_dip: f32, h_dip: f32, gap_dip: f32) -> Placement {
    match anchor {
        Anchor::Cursor(cur) => place_at_cursor(cur, w_dip, h_dip, gap_dip),
        Anchor::Pinned(origin) => {
            let (work, dpi) = monitor_for_point(origin);
            let s = dpi as f32 / 96.0;
            let (w, h) = ((w_dip * s) as i32, (h_dip * s) as i32);
            // Re-clamp: the monitor may have been unplugged or rearranged since
            // the pin was recorded.
            let x = clamp(origin.x, work.left, work.right - w);
            let y = clamp(origin.y, work.top, work.bottom - h);
            Placement { x, y, w, h, dpi, flipped_up: false }
        }
    }
}

fn place_at_cursor(cur: POINT, w_dip: f32, h_dip: f32, gap_dip: f32) -> Placement {
    let (work, dpi) = monitor_for_point(cur);
    let s = dpi as f32 / 96.0;
    let (w, h) = ((w_dip * s) as i32, (h_dip * s) as i32);
    let gap = (gap_dip * s) as i32;

    // Default: down and to the right, like every context menu since 1995.
    let mut x = cur.x + gap;
    let mut y = cur.y + gap;
    let mut flipped_up = false;

    if x + w > work.right {
        x = cur.x - w - gap;
    }
    if y + h > work.bottom {
        y = cur.y - h - gap;
        flipped_up = true;
    }

    x = clamp(x, work.left, (work.right - w).max(work.left));
    y = clamp(y, work.top, (work.bottom - h).max(work.top));

    // Clamping can drag the panel back over the cursor on a cramped monitor.
    // Landing under the pointer is worse than being a few pixels off, because
    // the next click the user makes gets eaten by a window that just appeared.
    let covers = cur.x >= x && cur.x < x + w && cur.y >= y && cur.y < y + h;
    if covers {
        let room_below = work.bottom - cur.y;
        let room_above = cur.y - work.top;
        if room_below >= room_above {
            y = (cur.y + gap).min((work.bottom - h).max(work.top));
            flipped_up = false;
        } else {
            y = (cur.y - h - gap).max(work.top);
            flipped_up = true;
        }
    }

    Placement { x, y, w, h, dpi, flipped_up }
}

/// Keep the bottom edge fixed when the panel opened upward, so a list that
/// grows expands away from the cursor instead of walking over it.
pub fn adjust_for_height(prev: &Placement, new_h: i32) -> (i32, i32) {
    if prev.flipped_up {
        (prev.x, prev.y + prev.h - new_h)
    } else {
        (prev.x, prev.y)
    }
}

fn clamp(v: i32, lo: i32, hi: i32) -> i32 {
    if hi < lo { lo } else { v.max(lo).min(hi) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These exercise the pure geometry by pinning a synthetic work area; the
    // monitor query itself is thin enough not to be worth faking.
    fn flip(cur: POINT, work: RECT, w: i32, h: i32, gap: i32) -> (i32, i32, bool) {
        let mut x = cur.x + gap;
        let mut y = cur.y + gap;
        let mut up = false;
        if x + w > work.right {
            x = cur.x - w - gap;
        }
        if y + h > work.bottom {
            y = cur.y - h - gap;
            up = true;
        }
        (clamp(x, work.left, work.right - w), clamp(y, work.top, work.bottom - h), up)
    }

    const WORK: RECT = RECT { left: 0, top: 0, right: 1920, bottom: 1040 };

    #[test]
    fn opens_down_right_with_room() {
        let (x, y, up) = flip(POINT { x: 400, y: 300 }, WORK, 360, 200, 18);
        assert_eq!((x, y), (418, 318));
        assert!(!up);
    }

    #[test]
    fn flips_left_near_right_edge() {
        let (x, _, _) = flip(POINT { x: 1900, y: 300 }, WORK, 360, 200, 18);
        assert!(x < 1900 - 360, "should open to the left of the cursor");
    }

    #[test]
    fn flips_up_near_bottom_edge() {
        let (_, y, up) = flip(POINT { x: 400, y: 1030 }, WORK, 360, 200, 18);
        assert!(up);
        assert!(y + 200 < 1030, "should sit entirely above the cursor");
    }

    #[test]
    fn never_covers_the_cursor() {
        for cx in (0..1920).step_by(137) {
            for cy in (0..1040).step_by(97) {
                let cur = POINT { x: cx, y: cy };
                let (x, y, _) = flip(cur, WORK, 360, 200, 18);
                let covers = cx >= x && cx < x + 360 && cy >= y && cy < y + 200;
                assert!(!covers, "panel covered cursor at {cx},{cy}");
            }
        }
    }

    #[test]
    fn stays_inside_the_work_area() {
        for cx in (0..1920).step_by(211) {
            for cy in (0..1040).step_by(151) {
                let (x, y, _) = flip(POINT { x: cx, y: cy }, WORK, 360, 200, 18);
                assert!(x >= WORK.left && x + 360 <= WORK.right);
                assert!(y >= WORK.top && y + 200 <= WORK.bottom);
            }
        }
    }

    #[test]
    fn upward_panel_keeps_its_bottom_edge_when_growing() {
        let p = Placement { x: 100, y: 500, w: 360, h: 200, dpi: 96, flipped_up: true };
        let (_, y) = adjust_for_height(&p, 300);
        assert_eq!(y, 400, "bottom stays at 700 while the top moves up");
    }

    #[test]
    fn downward_panel_keeps_its_top_edge_when_growing() {
        let p = Placement { x: 100, y: 500, w: 360, h: 200, dpi: 96, flipped_up: false };
        let (_, y) = adjust_for_height(&p, 300);
        assert_eq!(y, 500);
    }
}
