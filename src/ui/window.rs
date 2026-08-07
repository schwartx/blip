//! The panel window and its state machine.
//!
//! Style choices worth spelling out, because each one is load-bearing:
//!
//! * `WS_EX_NOACTIVATE` — the panel never takes focus. It can appear while
//!   you're mid-word in another window and you won't drop a keystroke. Mouse
//!   messages still arrive normally; only the keyboard is off limits, which is
//!   why Esc is a hotkey registered *only while the pointer is over the panel*
//!   rather than a global one that would swallow Esc from every other app.
//! * `WS_EX_NOREDIRECTIONBITMAP` — no GDI surface, so per-pixel alpha is
//!   composited by DWM. This is what makes the rounded corners and the shadow
//!   real rather than approximated against a background colour.
//! * `WS_EX_TOOLWINDOW` — keeps the panel out of Alt-Tab and the taskbar.
//!
//! The panel is strictly event-driven. The 60fps timer runs only while it is
//! visible or animating; when it's hidden there is no timer at all and the
//! process is genuinely idle.

use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Instant;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetLastInputInfo, LASTINPUTINFO, MOD_NOREPEAT, RegisterHotKey, ReleaseCapture, SetCapture,
    TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, UnregisterHotKey, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result};

use crate::config::Config;
use crate::ipc::pipe::PipeServer;
use crate::model::{Command, Level};
use crate::store::{Store, TickCtx};
use crate::ui::audio::Audio;
use crate::ui::autostart;
use crate::ui::layout::{Hit, Layout, Metrics};
use crate::ui::position::{self, Anchor, Placement};
use crate::ui::render::{Frame, Renderer};
use crate::ui::tray::{MenuAction, Tray};
use crate::ui::{FRAME_MS, WM_APP_IPC, WM_APP_TRAY};
use crate::wide;

const CLASS_NAME: &str = "BlipPanelWindow";
const TIMER_ANIM: usize = 1;
const TIMER_IDLE: usize = 2;
const HOTKEY_ESC: i32 = 0xB119;
/// Pointer travel in DIP before a press is reinterpreted as a drag rather than
/// a click. Everything on the panel is clickable, so the only way to keep both
/// "click the row" and "drag the panel" is to decide after the fact.
const DRAG_THRESHOLD: f32 = 4.0;
/// No keyboard or mouse for this long and we assume you walked away, which
/// pauses every countdown. Coming back to an empty panel would mean the
/// notifier quietly threw away the thing it was built to deliver.
const AWAY_MS: u32 = 30_000;
/// Lives in `Win32_UI_Controls`, which we'd otherwise have no use for.
const WM_MOUSELEAVE: u32 = 0x02A3;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    None,
    /// Button is down but the pointer hasn't moved far enough to commit.
    Armed { start: POINT, win: POINT, hit: Hit },
    Active { grab: POINT },
}

pub struct Panel {
    hwnd: HWND,
    cfg: Config,
    metrics: Metrics,
    store: Store,
    layout: Layout,

    renderer: Option<Renderer>,
    audio: Audio,
    tray: Option<Tray>,
    rx: Receiver<Command>,

    anchor: Anchor,
    place: Placement,
    /// Animated window height in DIP, lerped toward `layout.window_h`.
    anim_h: f32,

    visible: bool,
    alpha: f32,
    target_alpha: f32,
    scroll: f32,

    hit: Hit,
    mouse: POINT,
    captured: bool,
    esc_held: bool,
    drag: Drag,
    /// Counts down after the panel appears; clicks are ignored until it hits
    /// zero so a click already travelling toward whatever was underneath
    /// doesn't land on a window that just materialised there.
    grace: f32,

    last_tick: Instant,
    idle: f32,
    quit: bool,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(cfg: Config, rx: Receiver<Command>, wake: Arc<AtomicIsize>, _pipe: Arc<PipeServer>) -> Result<()> {
    // Before any window exists, or the OS hands us virtualised coordinates and
    // every position calculation downstream is subtly wrong on a scaled display.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let hinst = unsafe { GetModuleHandleW(None) }?;
    let class = wide(CLASS_NAME);

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst.into(),
        lpszClassName: PCWSTR(class.as_ptr()),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        ..Default::default()
    };
    unsafe { RegisterClassExW(&wc) };

    let metrics = Metrics::from_config(&cfg);
    let panel = Box::new(Panel {
        hwnd: HWND(std::ptr::null_mut()),
        cfg: cfg.clone(),
        metrics,
        store: Store::new(),
        layout: Layout::build(&[], &metrics, |_| 0.0),
        renderer: None,
        audio: Audio::new(&cfg),
        tray: None,
        rx,
        anchor: Anchor::Cursor(POINT { x: 0, y: 0 }),
        place: Placement { x: 0, y: 0, w: 0, h: 0, dpi: 96, flipped_up: false },
        anim_h: 0.0,
        visible: false,
        alpha: 0.0,
        target_alpha: 0.0,
        scroll: 0.0,
        hit: Hit::Outside,
        mouse: POINT::default(),
        captured: false,
        esc_held: false,
        drag: Drag::None,
        grace: 0.0,
        last_tick: Instant::now(),
        idle: 0.0,
        quit: false,
    });
    let raw = Box::into_raw(panel);

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            PCWSTR(class.as_ptr()),
            PCWSTR(wide("blip").as_ptr()),
            WS_POPUP,
            0,
            0,
            10,
            10,
            None,
            None,
            Some(hinst.into()),
            Some(raw as *const _),
        )
    }?;

    unsafe {
        let p = &mut *raw;
        p.hwnd = hwnd;
        p.tray = Some(Tray::new(hwnd, WM_APP_TRAY));
    }

    // Publish the HWND so IPC threads can wake the message loop. Anything that
    // arrived before this point is already queued and gets drained on the first
    // poke, so there's no race to lose.
    wake.store(hwnd.0 as isize, Ordering::Release);
    let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_IPC, WPARAM(0), LPARAM(0)) };

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    unsafe { drop(Box::from_raw(raw)) };
    Ok(())
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = lp.0 as *const CREATESTRUCTW;
        let ptr = unsafe { (*cs).lpCreateParams } as isize;
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr) };
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    }

    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Panel;
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    }
    let p = unsafe { &mut *ptr };

    match p.handle(msg, wp, lp) {
        Some(r) => r,
        None => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

// ---------------------------------------------------------------------------
// Message handling
// ---------------------------------------------------------------------------

impl Panel {
    fn scale(&self) -> f32 {
        self.place.dpi.max(96) as f32 / 96.0
    }

    /// Client pixel coords from an LPARAM, converted to DIP.
    fn lparam_dip(&self, lp: LPARAM) -> (f32, f32) {
        let x = (lp.0 & 0xFFFF) as i16 as f32;
        let y = ((lp.0 >> 16) & 0xFFFF) as i16 as f32;
        let s = self.scale();
        (x / s, y / s)
    }

    fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_APP_IPC => {
                self.drain_commands();
                Some(LRESULT(0))
            }

            WM_TIMER => {
                match wp.0 {
                    TIMER_ANIM => self.tick(),
                    TIMER_IDLE => self.idle_check(),
                    _ => {}
                }
                Some(LRESULT(0))
            }

            WM_MOUSEMOVE => {
                let (x, y) = self.lparam_dip(lp);
                self.mouse = POINT { x: x as i32, y: y as i32 };
                self.on_mouse_move(x, y);
                Some(LRESULT(0))
            }

            WM_MOUSELEAVE => {
                self.leave();
                Some(LRESULT(0))
            }

            WM_LBUTTONDOWN => {
                if self.grace <= 0.0 {
                    let (x, y) = self.lparam_dip(lp);
                    let hit = self.layout.hit(x, y, self.scroll);
                    let mut win = POINT::default();
                    let _ = unsafe { GetWindowRect(self.hwnd, &mut std::mem::zeroed()) };
                    win.x = self.place.x;
                    win.y = self.place.y;
                    self.drag = Drag::Armed { start: position::cursor_pos(), win, hit };
                    unsafe { SetCapture(self.hwnd) };
                    self.captured = true;
                }
                Some(LRESULT(0))
            }

            WM_LBUTTONUP => {
                self.on_mouse_up();
                Some(LRESULT(0))
            }

            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as f32;
                if self.layout.scroll_max > 0.5 {
                    self.scroll = (self.scroll - delta * 0.4).clamp(0.0, self.layout.scroll_max);
                    self.request_frame();
                }
                Some(LRESULT(0))
            }

            WM_HOTKEY if wp.0 as i32 == HOTKEY_ESC => {
                self.begin_hide();
                Some(LRESULT(0))
            }

            WM_APP_TRAY => {
                self.on_tray(crate::ui::tray::tray_event(lp));
                Some(LRESULT(0))
            }

            WM_DPICHANGED => {
                let dpi = (wp.0 & 0xFFFF) as u32;
                self.place.dpi = dpi.max(96);
                if let Some(r) = self.renderer.as_mut() {
                    let (mw, mh) = max_surface(&self.metrics);
                    let _ = r.ensure(self.place.dpi, mw, mh);
                }
                self.reposition(true);
                self.request_frame();
                Some(LRESULT(0))
            }

            WM_DISPLAYCHANGE => {
                // A monitor was added, removed or rearranged — a pinned origin
                // may now point into empty space.
                self.reposition(true);
                Some(LRESULT(0))
            }

            WM_DESTROY => {
                self.tray = None;
                unsafe { PostQuitMessage(0) };
                Some(LRESULT(0))
            }

            _ => None,
        }
    }

    // -- IPC ---------------------------------------------------------------

    fn drain_commands(&mut self) {
        let mut popped = false;
        let mut sound: Option<Level> = None;
        let mut quit = false;

        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                Command::Notify(req) => {
                    let cfg = self.cfg.clone();
                    let a = self.store.push(req, &cfg);
                    if a.pop {
                        popped = true;
                    }
                    if a.sound {
                        sound = Some(a.level);
                    }
                }
                Command::Dismiss { id } => self.store.dismiss_id(&id),
                Command::Clear => {
                    self.store.clear();
                    self.begin_hide();
                }
                Command::Show => popped = true,
                Command::Ping => {}
                // Deferred rather than torn down here: `DestroyWindow` dispatches
                // WM_DESTROY synchronously, which would re-enter the wndproc
                // while we are still inside this loop holding `&mut self`.
                Command::Quit => {
                    quit = true;
                    break;
                }
            }
        }

        if quit {
            self.quit = true;
            let _ = unsafe { DestroyWindow(self.hwnd) };
            return;
        }

        if let Some(level) = sound {
            self.audio.play(level);
        }
        if popped {
            self.show();
        }
        self.refresh_tray();
        self.request_frame();
    }

    /// Post a notification about blip itself.
    ///
    /// A fixed id per message means repeated toggling updates one row in place
    /// instead of stacking — the same rule everything else here follows.
    fn notify_self(&mut self, title: &str, body: Option<String>, level: Level) {
        let cfg = self.cfg.clone();
        let a = self.store.push(
            crate::model::NotifyRequest {
                title: title.into(),
                body,
                level: Some(level),
                id: Some("blip-self".into()),
                source: Some("blip".into()),
                ..Default::default()
            },
            &cfg,
        );
        if a.sound {
            self.audio.play(a.level);
        }
        self.show();
        self.refresh_tray();
        self.request_frame();
    }

    // -- show / hide -------------------------------------------------------

    fn show(&mut self) {
        if self.renderer.is_none() && self.build_renderer().is_err() {
            return;
        }

        // Capture the cursor position once, here. Reading it every frame would
        // make the panel chase the mouse around the screen.
        if !self.pinned() {
            self.anchor = Anchor::Cursor(position::cursor_pos());
        }

        self.relayout();
        if !self.visible {
            self.anim_h = self.layout.window_h;
            self.scroll = 0.0;
        }
        self.reposition(true);

        if !self.visible {
            let _ = unsafe { ShowWindow(self.hwnd, SW_SHOWNOACTIVATE) };
            self.visible = true;
            self.grace = self.cfg.behavior.input_grace;
        }
        self.target_alpha = 1.0;
        self.idle = 0.0;
        self.start_anim();
    }

    /// User-initiated open. Distinct from `show()`, which is what an arriving
    /// notification calls: by then there is always something to display.
    fn show_if_any(&mut self) {
        if self.store.live_count() > 0 {
            self.show();
        }
    }

    fn begin_hide(&mut self) {
        if !self.visible {
            return;
        }
        self.target_alpha = 0.0;
        self.start_anim();
    }

    fn finish_hide(&mut self) {
        let _ = unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        self.visible = false;
        self.alpha = 0.0;
        self.release_esc();
        self.release_capture();
        self.hit = Hit::Outside;
        self.drag = Drag::None;
        let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_ANIM) };
        // Switch to the slow timer so the GPU devices can be released after a
        // long quiet spell without holding a 60fps clock open to notice.
        unsafe { SetTimer(Some(self.hwnd), TIMER_IDLE, 2000, None) };
    }

    fn pinned(&self) -> bool {
        matches!(self.anchor, Anchor::Pinned(_))
    }

    // -- geometry ----------------------------------------------------------

    fn relayout(&mut self) {
        let m = self.metrics;
        let text_w = m.row_text_width();
        let items = &self.store.items;

        let heights: Vec<f32> = match self.renderer.as_mut() {
            Some(r) => items
                .iter()
                .map(|n| r.measure_body(n, text_w, m.body_max_lines, m.body_line_h))
                .collect(),
            None => vec![0.0; items.len()],
        };

        self.layout = Layout::build(items, &m, |i| heights.get(i).copied().unwrap_or(0.0));
        self.scroll = self.scroll.clamp(0.0, self.layout.scroll_max);
    }

    fn reposition(&mut self, snap: bool) {
        let h = if snap { self.layout.window_h } else { self.anim_h };
        if snap {
            self.anim_h = h;
        }
        let p = position::place(self.anchor, self.layout.window_w, h, self.cfg.behavior.cursor_gap);
        let (x, y) = if snap {
            (p.x, p.y)
        } else {
            // Preserve which edge is anchored so a growing list expands away
            // from the cursor rather than walking over it.
            position::adjust_for_height(&self.place, p.h)
        };
        self.place = Placement { x, y, ..p };

        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                self.place.x,
                self.place.y,
                self.place.w,
                self.place.h,
                SWP_NOACTIVATE,
            );
        }
    }

    // -- frame -------------------------------------------------------------

    fn start_anim(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_IDLE);
            SetTimer(Some(self.hwnd), TIMER_ANIM, FRAME_MS, None);
        }
        self.last_tick = Instant::now();
    }

    fn request_frame(&mut self) {
        if self.visible {
            self.start_anim();
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last_tick).as_secs_f32().clamp(0.0, 0.1);
        self.last_tick = now;

        self.grace = (self.grace - dt).max(0.0);

        let step = dt / self.cfg.behavior.anim.max(0.02);
        if self.alpha < self.target_alpha {
            self.alpha = (self.alpha + step).min(self.target_alpha);
        } else if self.alpha > self.target_alpha {
            self.alpha = (self.alpha - step).max(self.target_alpha);
        }

        // The whole TTL policy lives in these four flags.
        let ctx = TickCtx {
            panel_visible: self.visible && self.alpha > 0.6,
            hovered: !matches!(self.hit, Hit::Outside),
            session_locked: user_away(),
            anim: self.cfg.behavior.anim,
        };

        let layout = self.layout.clone();
        let scroll = self.scroll;
        let changed = self.store.tick(dt, &ctx, |i| {
            layout.rows.get(i).is_some_and(|g| layout.is_on_screen(g, scroll))
        });

        if changed {
            self.relayout();
            self.refresh_tray();
        }

        // Ease the window height toward the laid-out height so rows arriving or
        // leaving don't make the panel jump.
        let target_h = self.layout.window_h;
        if (self.anim_h - target_h).abs() > 0.3 {
            let k = (dt / 0.12).min(1.0);
            self.anim_h += (target_h - self.anim_h) * k;
            self.reposition(false);
        } else if (self.anim_h - target_h).abs() > 0.0 {
            self.anim_h = target_h;
            self.reposition(false);
        }

        if self.visible && self.store.is_empty() && self.target_alpha > 0.0 {
            self.begin_hide();
        }

        self.render();

        if self.target_alpha <= 0.0 && self.alpha <= 0.001 {
            self.finish_hide();
        }
    }

    fn render(&mut self) {
        if !self.visible {
            return;
        }
        let Some(r) = self.renderer.as_mut() else { return };

        let (hover_row, hover_close) = match self.hit {
            Hit::Row(i, _) => (Some(i), false),
            Hit::RowClose(i, _) => (Some(i), true),
            _ => (None, false),
        };

        let frame = Frame {
            layout: &self.layout,
            items: &self.store.items,
            scroll: self.scroll,
            hover_row,
            hover_close,
            hover_button: matches!(self.hit, Hit::DismissAll),
            alpha: ease(self.alpha),
            count: self.store.len(),
        };
        let _ = r.draw(&frame);
        r.gc(&self.store.items);
    }

    fn build_renderer(&mut self) -> Result<()> {
        let dpi = unsafe { GetDpiForWindow(self.hwnd) }.max(96);
        self.place.dpi = dpi;
        let (mw, mh) = max_surface(&self.metrics);
        self.renderer = Some(Renderer::new(self.hwnd, &self.cfg, dpi, mw, mh)?);
        Ok(())
    }

    fn idle_check(&mut self) {
        self.idle += 2.0;
        if self.idle >= self.cfg.behavior.idle_release && self.renderer.is_some() {
            // Give the GPU stack back. The process, the pipe and the HTTP
            // listener stay up; the next notification pays a one-off rebuild
            // instead of every notification paying a process spawn.
            self.renderer = None;

            // Dropping the COM objects frees the allocations but leaves the
            // pages in our working set — Windows has no reason to reclaim them
            // from a process that isn't under memory pressure. For something
            // that lives in the tray all day and gets judged in Task Manager,
            // asking for the trim explicitly is worth the one syscall.
            unsafe {
                let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
            }

            let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_IDLE) };
        }
    }

    // -- mouse -------------------------------------------------------------

    fn on_mouse_move(&mut self, x: f32, y: f32) {
        if let Drag::Armed { start, win, hit } = self.drag {
            let now = position::cursor_pos();
            let s = self.scale();
            let moved = (((now.x - start.x).pow(2) + (now.y - start.y).pow(2)) as f32).sqrt();
            if moved > DRAG_THRESHOLD * s {
                self.drag = Drag::Active {
                    grab: POINT { x: now.x - win.x, y: now.y - win.y },
                };
            } else {
                let _ = hit;
            }
        }

        if let Drag::Active { grab } = self.drag {
            let now = position::cursor_pos();
            self.place.x = now.x - grab.x;
            self.place.y = now.y - grab.y;
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    self.place.x,
                    self.place.y,
                    self.place.w,
                    self.place.h,
                    SWP_NOACTIVATE,
                );
            }
            return;
        }

        let inside = x >= 0.0
            && y >= 0.0
            && x <= self.layout.window_w
            && y <= self.layout.window_h;

        if !inside && self.captured {
            self.leave();
            return;
        }

        let hit = self.layout.hit(x, y, self.scroll);
        if hit != self.hit {
            self.hit = hit;
            self.request_frame();
        }

        match hit {
            Hit::Outside => self.leave(),
            _ => {
                self.enter();
                if !self.captured {
                    // Track leaving so hover state doesn't get stuck on when the
                    // pointer exits without another move message.
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = unsafe { TrackMouseEvent(&mut tme) };
                }
            }
        }
    }

    fn enter(&mut self) {
        // Esc is registered only while the pointer is on the panel. A global Esc
        // hotkey would silently steal the key from whatever app the user is
        // actually working in for the several seconds the panel is up.
        if !self.esc_held {
            let ok = unsafe { RegisterHotKey(Some(self.hwnd), HOTKEY_ESC, MOD_NOREPEAT, VK_ESCAPE.0 as u32) };
            self.esc_held = ok.is_ok();
        }
        // Capture only when there is something to scroll: a captured mouse eats
        // clicks meant for other windows, so don't take it unless it buys a
        // feature the user can actually use.
        if !self.captured && self.layout.scroll_max > 0.5 {
            unsafe { SetCapture(self.hwnd) };
            self.captured = true;
        }
    }

    fn leave(&mut self) {
        if self.hit != Hit::Outside {
            self.hit = Hit::Outside;
            self.request_frame();
        }
        self.release_esc();
        if !matches!(self.drag, Drag::Active { .. }) {
            self.release_capture();
        }
    }

    fn release_esc(&mut self) {
        if self.esc_held {
            let _ = unsafe { UnregisterHotKey(Some(self.hwnd), HOTKEY_ESC) };
            self.esc_held = false;
        }
    }

    fn release_capture(&mut self) {
        if self.captured {
            let _ = unsafe { ReleaseCapture() };
            self.captured = false;
        }
    }

    fn on_mouse_up(&mut self) {
        let drag = self.drag;
        self.drag = Drag::None;
        self.release_capture();

        match drag {
            // Moved past the threshold: this was a drag, and dragging is the
            // user saying "put it here". Pin it.
            Drag::Active { .. } => {
                if self.cfg.behavior.drag_to_pin {
                    self.anchor = Anchor::Pinned(POINT { x: self.place.x, y: self.place.y });
                    self.refresh_tray();
                }
            }
            Drag::Armed { hit, .. } => self.activate(hit),
            Drag::None => {}
        }
        self.request_frame();
    }

    fn activate(&mut self, hit: Hit) {
        match hit {
            // One action, one meaning: "I've seen everything." Clearing without
            // hiding would just leave an empty panel on screen.
            Hit::DismissAll => {
                self.store.clear();
                self.begin_hide();
                self.refresh_tray();
            }
            Hit::RowClose(_, key) => {
                self.store.dismiss_key(key);
                self.refresh_tray();
            }
            // Clicking a row dismisses it, running its action first if it has
            // one. Anywhere on the row works, which is the point: getting rid of
            // something you've read should never require aiming at a small
            // glyph. The ✕ stays for the case where you want to drop a row
            // *without* firing its action.
            Hit::Row(idx, key) => {
                if let Some(cmd) = self.store.items.get(idx).and_then(|n| n.action.clone()) {
                    run_action(&cmd);
                }
                self.store.dismiss_key(key);
                self.refresh_tray();
            }
            _ => {}
        }
    }

    // -- tray --------------------------------------------------------------

    fn refresh_tray(&mut self) {
        let n = self.store.live_count();
        let pinned = self.pinned();
        if let Some(t) = self.tray.as_ref() {
            t.update(WM_APP_TRAY, n, pinned);
        }
    }

    fn on_tray(&mut self, event: u32) {
        match event {
            // Opening an empty panel is indistinguishable from a broken click:
            // it would appear and immediately auto-hide again, because "nothing
            // left to show" is the same condition that dismisses it.
            WM_LBUTTONUP => self.show_if_any(),
            WM_LBUTTONDBLCLK => {
                self.anchor = Anchor::Cursor(position::cursor_pos());
                self.refresh_tray();
                self.show_if_any();
            }
            WM_RBUTTONUP | WM_CONTEXTMENU => {
                let pinned = self.pinned();
                let count = self.store.live_count();
                let action = match self.tray.as_ref() {
                    Some(t) => t.show_menu(pinned, count, autostart::is_enabled()),
                    None => MenuAction::None,
                };
                match action {
                    MenuAction::Show => self.show_if_any(),
                    MenuAction::ClearAll => {
                        self.store.clear();
                        self.begin_hide();
                        self.refresh_tray();
                    }
                    MenuAction::ResetPosition => {
                        self.anchor = Anchor::Cursor(position::cursor_pos());
                        self.refresh_tray();
                    }
                    MenuAction::OpenConfig => {
                        if !Config::path().exists() {
                            let _ = Config::write_default();
                        }
                        open_in_editor(&Config::path());
                    }
                    // Report through the panel rather than a message box: a
                    // toggle that silently didn't take is worse than a noisy
                    // failure, and we already own the notification surface.
                    MenuAction::ToggleAutostart => {
                        let want = !autostart::is_enabled();
                        if autostart::set(want) {
                            self.notify_self(
                                if want { "已设为开机自动启动" } else { "已取消开机自动启动" },
                                None,
                                Level::Normal,
                            );
                        } else {
                            self.notify_self(
                                "无法修改开机自动启动",
                                Some("写入 HKCU\\...\\CurrentVersion\\Run 失败".into()),
                                Level::Critical,
                            );
                        }
                    }
                    MenuAction::Quit => {
                        self.quit = true;
                        let _ = unsafe { DestroyWindow(self.hwnd) };
                    }
                    MenuAction::None => {}
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Largest surface the panel can ever need, so the swapchain is allocated once
/// and content changes only ever move the window, never the buffer.
fn max_surface(m: &Metrics) -> (f32, f32) {
    let rows = m.max_visible_rows as f32;
    let h = rows * (m.row_min_h + m.body_line_h * m.body_max_lines as f32)
        + m.footer_h
        + m.pad * 2.0
        + 40.0;
    (m.width + m.pad * 2.0, h)
}

fn user_away() -> bool {
    let mut lii = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if unsafe { GetLastInputInfo(&mut lii) }.as_bool() {
        unsafe { GetTickCount() }.wrapping_sub(lii.dwTime) > AWAY_MS
    } else {
        false
    }
}

/// Smoothstep, so fades don't start and stop abruptly.
fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Run a user-supplied `--action` through the shell.
///
/// `raw_arg`, not `arg`: Rust quotes arguments per `CommandLineToArgvW`, but
/// `cmd.exe` does not parse its command line by those rules, so any action
/// containing a quote or a path with spaces arrives mangled. `/S` tells cmd to
/// strip exactly the outer quote pair and take the rest verbatim, which is the
/// only reliable way to hand it an arbitrary string.
fn run_action(cmd: &str) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut c = std::process::Command::new("cmd.exe");
    c.raw_arg(format!("/S /C \"{cmd}\""));
    c.creation_flags(CREATE_NO_WINDOW);
    let _ = c.spawn();
}

/// Open a file for editing, without ever getting stuck on a dialog.
///
/// `.toml` has no registered handler on a stock Windows install, and `start ""`
/// on an unassociated file doesn't fail — it puts up a modal "how do you want to
/// open this file?" picker. From a tray menu that reads as the app hanging.
/// So: ask the shell first, and fall back to Notepad, which is always present.
fn open_in_editor(path: &std::path::Path) {
    use windows::Win32::UI::Shell::ShellExecuteW;

    let file = wide(&path.to_string_lossy());
    let verb = wide("open");
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };

    // ShellExecuteW returns a pseudo-HINSTANCE; anything <= 32 is an error code,
    // and the one we expect here is ERROR_NO_ASSOCIATION (31).
    if result.0 as usize > 32 {
        return;
    }

    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("notepad.exe")
        .arg(path)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}
