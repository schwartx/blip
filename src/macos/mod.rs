//! Native macOS notification panel. All AppKit objects stay on the main thread;
//! IPC workers send the same commands used by the Windows policy engine.

mod position;

use std::cell::{Cell, RefCell};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::{Arc, mpsc::Receiver};
use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::{
    MainThreadMarker, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSRunLoop,
    NSRunLoopCommonModes, NSSize, NSString, NSTimer, ns_string,
};

use crate::config::{Config, parse_color};
use crate::ipc::pipe::PipeServer;
use crate::model::{Command, Level, Notification};
use crate::store::{Store, TickCtx};
use position::{Point, WorkArea};

const HEADER_HEIGHT: f64 = 34.0;
const FOOTER_HEIGHT: f64 = 40.0;
const AWAY_SECONDS: f64 = 30.0;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state_id: i32, event_type: u32) -> f64;
}

fn idle_seconds() -> f64 {
    // kCGEventSourceStateCombinedSessionState and kCGAnyInputEventType. This
    // public query reads elapsed input time; it does not install a keyboard tap.
    unsafe { CGEventSourceSecondsSinceLastEventType(0, u32::MAX) }
}

// SAFETY: These subclasses preserve AppKit's main-thread restriction, have no
// custom destructor, and use the documented Objective-C method signatures.
define_class!(
    #[unsafe(super = NSPanel)]
    #[thread_kind = MainThreadOnly]
    struct BlipPanel;

    unsafe impl NSObjectProtocol for BlipPanel {}

    impl BlipPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool { false }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool { false }
    }
);

impl BlipPanel {
    fn new(mtm: MainThreadMarker, width: f64) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSPanel's designated initializer has this exact signature.
        unsafe {
            msg_send![super(this),
                initWithContentRect: rect(0.0, 0.0, width, 120.0),
                styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        }
    }
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    struct FlippedView;

    unsafe impl NSObjectProtocol for FlippedView {}

    impl FlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool { true }
    }
);

impl FlippedView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        // SAFETY: NSView's designated initializer has this exact signature.
        unsafe { msg_send![super(Self::alloc(mtm).set_ivars(())), initWithFrame: frame] }
    }
}

#[derive(Default)]
struct DragIvars {
    armed: Cell<Option<NSPoint>>,
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = DragIvars]
    struct DragHandle;

    unsafe impl NSObjectProtocol for DragHandle {}

    impl DragHandle {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool { true }

        #[unsafe(method(acceptsFirstMouse:))]
        fn first_mouse(&self, _event: Option<&NSEvent>) -> bool { true }

        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<&NSView> {
            // Hit-test coordinates are in the superview, whose origin is flipped.
            if contains(self.frame(), point) { let view: &NSView = self; Some(view) } else { None }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if let Some(window) = self.window() {
                self.ivars().armed.set(Some(window.frame().origin));
                window.performWindowDragWithEvent(event);
            }
        }
    }
);

impl DragHandle {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DragIvars::default());
        // SAFETY: NSView's designated initializer has this exact signature.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

define_class!(
    #[unsafe(super = NSButton)]
    #[thread_kind = MainThreadOnly]
    struct RowButton;

    unsafe impl NSObjectProtocol for RowButton {}

    impl RowButton {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool { true }

        #[unsafe(method(acceptsFirstMouse:))]
        fn first_mouse(&self, _event: Option<&NSEvent>) -> bool { true }

        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<&NSView> {
            // Labels and progress bars remain part of the row's click target.
            if contains(self.frame(), point) { let view: &NSView = self; Some(view) } else { None }
        }
    }
);

impl RowButton {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        // SAFETY: NSButton inherits NSView's frame initializer.
        unsafe { msg_send![super(Self::alloc(mtm).set_ivars(())), initWithFrame: frame] }
    }
}

struct Row {
    key: u64,
    view: Retained<RowButton>,
}

struct State {
    cfg: Config,
    rx: Receiver<Command>,
    local: Arc<PipeServer>,
    store: Store,
    panel: Retained<BlipPanel>,
    root: Retained<FlippedView>,
    header: Retained<DragHandle>,
    scroll: Retained<NSScrollView>,
    footer: Retained<NSButton>,
    status: Retained<NSStatusItem>,
    quiet_menu: Retained<NSMenuItem>,
    rows: Vec<Row>,
    sound: Option<Retained<NSSound>>,
    anchor: Point,
    pinned: bool,
    quiet: bool,
    session_inactive: bool,
    display_asleep: bool,
    system_asleep: bool,
    grace_until: Option<Instant>,
    last_tick: Instant,
    quit: bool,
}

struct ControllerIvars {
    state: RefCell<State>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ControllerIvars]
    struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSApplicationDelegate for Controller {
        #[unsafe(method(applicationShouldTerminate:))]
        fn application_should_terminate(&self, _app: &NSApplication) -> NSApplicationTerminateReply {
            // Allow macOS to complete logout/shutdown immediately. The OS
            // closes descriptors and releases the instance lock; the next
            // launch safely recovers any socket left by process termination.
            // Menu and CLI quits use request_quit's full Rust cleanup path.
            self.ivars().state.borrow().local.stop();
            NSApplicationTerminateReply::TerminateNow
        }
    }

    impl Controller {
        #[unsafe(method(tick:))]
        fn timer_tick(&self, _timer: &NSTimer) { self.tick(); }

        #[unsafe(method(showPanel:))]
        fn show_panel(&self, _sender: Option<&AnyObject>) {
            let mut state = self.ivars().state.borrow_mut();
            state.show(true);
        }

        #[unsafe(method(hidePanel:))]
        fn hide_panel(&self, _sender: Option<&AnyObject>) {
            self.ivars().state.borrow().panel.orderOut(None);
        }

        #[unsafe(method(clearPanel:))]
        fn clear_panel(&self, _sender: Option<&AnyObject>) {
            let mut state = self.ivars().state.borrow_mut();
            state.store.clear();
            state.panel.orderOut(None);
            state.rebuild(self);
        }

        #[unsafe(method(resetPosition:))]
        fn reset_position(&self, _sender: Option<&AnyObject>) {
            let mut state = self.ivars().state.borrow_mut();
            state.pinned = false;
            state.header.ivars().armed.set(None);
            state.show(true);
        }

        #[unsafe(method(toggleQuiet:))]
        fn toggle_quiet(&self, _sender: Option<&AnyObject>) {
            let mut state = self.ivars().state.borrow_mut();
            state.quiet = !state.quiet;
            state.quiet_menu.setState(if state.quiet { 1 } else { 0 });
        }

        #[unsafe(method(openConfig:))]
        fn open_config(&self, _sender: Option<&AnyObject>) {
            let path = Config::path();
            if !path.exists() && let Err(error) = Config::write_default() {
                eprintln!("could not create configuration: {error}");
                return;
            }
            spawn_and_reap(ProcessCommand::new("/usr/bin/open").arg("-t").arg(path));
        }

        #[unsafe(method(rowClicked:))]
        fn row_clicked(&self, sender: &NSButton) {
            let mut state = self.ivars().state.borrow_mut();
            let key = sender.tag() as u64;
            if let Some(row) = state.store.items.iter().find(|n| n.key == key && !n.dying) {
                if let Some(action) = &row.action {
                    spawn_and_reap(ProcessCommand::new("/bin/sh").arg("-c").arg(action));
                }
                state.store.dismiss_key(key);
            }
        }

        #[unsafe(method(quitBlip:))]
        fn quit_blip(&self, _sender: Option<&AnyObject>) { self.request_quit(); }

        #[unsafe(method(workspaceChanged:))]
        fn workspace_changed(&self, notification: &NSNotification) {
            let name = notification.name();
            let mut state = self.ivars().state.borrow_mut();
            // SAFETY: These are Foundation notification-name constants. AppKit
            // workspace notifications are delivered on the main run loop.
            unsafe {
                if *name == *NSWorkspaceSessionDidResignActiveNotification { state.session_inactive = true; }
                if *name == *NSWorkspaceSessionDidBecomeActiveNotification { state.session_inactive = false; }
                if *name == *NSWorkspaceScreensDidSleepNotification { state.display_asleep = true; }
                if *name == *NSWorkspaceScreensDidWakeNotification { state.display_asleep = false; }
                if *name == *NSWorkspaceWillSleepNotification { state.system_asleep = true; }
                if *name == *NSWorkspaceDidWakeNotification { state.system_asleep = false; }
            }
            // Sleep must never be charged to a notification's lifetime.
            state.last_tick = Instant::now();
        }
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker, state: State) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ControllerIvars {
            state: RefCell::new(state),
        });
        // SAFETY: NSObject's init method has this exact signature.
        unsafe { msg_send![super(this), init] }
    }

    fn request_quit(&self) {
        let mut state = self.ivars().state.borrow_mut();
        state.quit = true;
        state.local.stop();
        state.panel.orderOut(None);
        let app = NSApplication::sharedApplication(self.mtm());
        app.stop(None);
        // stop: takes effect after the next event. Posting an application event
        // also handles a quit received while no windows are visible.
        if let Some(event) = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined, NSPoint::new(0.0, 0.0), NSEventModifierFlags::empty(),
            0.0, 0, None, 0, 0, 0,
        ) { app.postEvent_atStart(&event, true); }
    }

    fn tick(&self) {
        let mut state = self.ivars().state.borrow_mut();
        if state.quit {
            return;
        }
        let now = Instant::now();
        let dt = now.duration_since(state.last_tick).as_secs_f32().min(0.25);
        state.last_tick = now;
        let frame = state.panel.frame();
        let pressed = NSEvent::pressedMouseButtons() != 0;
        if let Some(start) = state.header.ivars().armed.get() {
            if state.cfg.behavior.drag_to_pin
                && (start.x - frame.origin.x).abs() + (start.y - frame.origin.y).abs() > 1.0
            {
                state.pinned = true;
            }
            if !pressed {
                state.header.ivars().armed.set(None);
            }
        }
        if state.pinned {
            state.anchor = Point {
                x: frame.origin.x,
                y: frame.origin.y + frame.size.height,
            };
        }
        if state.grace_until.is_some_and(|deadline| now >= deadline) {
            state.panel.setIgnoresMouseEvents(false);
            state.grace_until = None;
        }

        // Leave native button tracking and window dragging undisturbed. A
        // content update arriving during a press waits until mouse release.
        if pressed
            && (state.header.ivars().armed.get().is_some()
                || contains(state.panel.frame(), NSEvent::mouseLocation()))
        {
            return;
        }

        // Bound each drain so an HTTP progress flood cannot starve AppKit input.
        let mut rebuild = false;
        for _ in 0..256 {
            let Ok(command) = state.rx.try_recv() else {
                break;
            };
            match command {
                Command::Notify(request) => {
                    let if_idle = request.if_idle;
                    let sound_override = request.sound.clone();
                    let config = state.cfg.clone();
                    let arrival = state.store.push(request, &config);
                    rebuild = true;
                    let allowed = !state.quiet || arrival.level == Level::Critical;
                    let idle_allowed =
                        if_idle.is_none_or(|seconds| idle_seconds() >= f64::from(seconds));
                    if arrival.pop && allowed && idle_allowed {
                        state.show(false);
                    }
                    if arrival.sound && allowed && idle_allowed {
                        state.play_sound(arrival.level, sound_override.as_deref());
                    }
                }
                Command::Dismiss { id } => state.store.dismiss_id(&id),
                Command::Clear => {
                    state.store.clear();
                    state.panel.orderOut(None);
                    rebuild = true;
                }
                Command::Show => state.show(true),
                Command::Ping => {}
                Command::Quit => {
                    drop(state);
                    self.request_quit();
                    return;
                }
            }
        }
        if rebuild {
            state.rebuild(self);
        }

        let frame = state.panel.frame();
        let hovered = contains(frame, NSEvent::mouseLocation());
        let viewport = state.scroll.documentVisibleRect();
        let visible_rows: Vec<bool> = state
            .rows
            .iter()
            .map(|row| intersects(row.view.frame(), viewport))
            .collect();
        let ctx = TickCtx {
            panel_visible: state.panel.isVisible()
                && state
                    .panel
                    .occlusionState()
                    .contains(NSWindowOcclusionState::Visible),
            hovered,
            session_locked: state.session_inactive
                || state.display_asleep
                || state.system_asleep
                || idle_seconds() >= AWAY_SECONDS,
            anim: state.cfg.behavior.anim,
        };
        let had_items = !state.store.is_empty();
        state
            .store
            .tick(dt, &ctx, |i| visible_rows.get(i).copied().unwrap_or(false));
        if state
            .rows
            .iter()
            .map(|row| row.key)
            .ne(state.store.items.iter().map(|n| n.key))
        {
            state.rebuild(self);
        }
        for (row, notification) in state.rows.iter().zip(&state.store.items) {
            row.view
                .setAlphaValue(f64::from(notification.appear * (1.0 - notification.fade)));
        }
        if state.store.is_empty() && had_items {
            state.panel.orderOut(None);
        }
        state.update_badge(self.mtm());
        if let Some(sound) = &state.sound
            && !sound.isPlaying()
        {
            state.sound = None;
        }
    }
}

impl State {
    fn show(&mut self, force: bool) {
        let was_visible = self.panel.isVisible();
        if !self.pinned && (!was_visible || force) {
            let cursor = NSEvent::mouseLocation();
            self.anchor = Point {
                x: cursor.x,
                y: cursor.y,
            };
        }
        self.place();
        if !was_visible {
            let grace = self.cfg.behavior.input_grace.clamp(0.0, 10.0);
            self.panel.setIgnoresMouseEvents(grace > 0.0);
            self.grace_until = Some(Instant::now() + std::time::Duration::from_secs_f32(grace));
        }
        self.panel.orderFrontRegardless();
    }

    fn place(&self) {
        let work = screen_for(self.anchor, self.panel.mtm());
        let size = self.panel.frame().size;
        let origin = if self.pinned {
            position::clamp(
                Point {
                    x: self.anchor.x,
                    y: self.anchor.y - size.height,
                },
                size.width,
                size.height,
                work,
            )
        } else {
            position::near_cursor(
                self.anchor,
                size.width,
                size.height,
                f64::from(self.cfg.behavior.cursor_gap.max(0.0)),
                work,
            )
        };
        self.panel.setFrameOrigin(NSPoint::new(origin.x, origin.y));
    }

    fn rebuild(&mut self, controller: &Controller) {
        let mtm = controller.mtm();
        let work = screen_for(self.anchor, mtm);
        let width = f64::from(self.cfg.width)
            .clamp(240.0, 1000.0)
            .min(work.width);
        let old_scroll = self.scroll.documentVisibleRect().origin;
        let document = FlippedView::new(mtm, rect(0.0, 0.0, width, 1.0));
        self.rows.clear();
        let mut y = 0.0;
        let mut visible_height = 0.0;
        for (index, notification) in self.store.items.iter().enumerate() {
            let row = build_row(notification, &self.cfg, width, y, controller);
            let height = row.frame().size.height;
            add(&document, &row);
            self.rows.push(Row {
                key: notification.key,
                view: row,
            });
            y += height;
            if index < self.cfg.max_visible_rows.max(1) {
                visible_height += height;
            }
        }
        if self.store.is_empty() {
            let empty = label(
                "No notifications",
                &self.cfg,
                13.0,
                &self.cfg.theme.body,
                mtm,
            );
            empty.setFrame(rect(14.0, 14.0, width - 28.0, 24.0));
            add(&document, &empty);
            y = 52.0;
            visible_height = y;
        }
        let list_height = visible_height
            .max(44.0)
            .min((work.height - HEADER_HEIGHT - FOOTER_HEIGHT - 24.0).max(44.0));
        let height = HEADER_HEIGHT + list_height + FOOTER_HEIGHT;
        document.setFrameSize(NSSize::new(width, y));
        self.panel.setContentSize(NSSize::new(width, height));
        self.root.setFrame(rect(0.0, 0.0, width, height));
        self.header.setFrame(rect(0.0, 0.0, width, HEADER_HEIGHT));
        self.scroll
            .setFrame(rect(0.0, HEADER_HEIGHT, width, list_height));
        self.scroll.setDocumentView(Some(&document));
        self.scroll.contentView().scrollToPoint(NSPoint::new(
            0.0,
            old_scroll.y.min((y - list_height).max(0.0)).max(0.0),
        ));
        self.scroll
            .reflectScrolledClipView(&self.scroll.contentView());
        self.footer
            .setFrame(rect(8.0, height - FOOTER_HEIGHT + 5.0, width - 16.0, 30.0));
        self.place();
        self.update_badge(mtm);
    }

    fn update_badge(&self, mtm: MainThreadMarker) {
        if let Some(button) = self.status.button(mtm) {
            let count = self.store.live_count();
            let title = if count == 0 {
                "Blip".to_string()
            } else {
                format!("Blip {count}")
            };
            if button.title().to_string() != title {
                button.setTitle(&NSString::from_str(&title));
            }
        }
    }

    fn play_sound(&mut self, level: Level, override_path: Option<&str>) {
        if !self.cfg.sound.enabled {
            return;
        }
        let configured = match level {
            Level::Low => &self.cfg.sound.low,
            Level::Normal => &self.cfg.sound.normal,
            Level::Critical => &self.cfg.sound.critical,
        };
        let path = override_path.unwrap_or(configured);
        let sound = if path.is_empty() {
            NSSound::soundNamed(ns_string!("Ping"))
        } else {
            NSSound::initWithContentsOfFile_byReference(
                NSSound::alloc(),
                &NSString::from_str(path),
                true,
            )
        };
        if let Some(sound) = sound {
            sound.play();
            self.sound = Some(sound);
        }
    }
}

fn build_row(
    notification: &Notification,
    cfg: &Config,
    width: f64,
    y: f64,
    controller: &Controller,
) -> Retained<RowButton> {
    let mtm = controller.mtm();
    let row = RowButton::new(mtm, rect(0.0, y, width, 1.0));
    row.setTitle(ns_string!(""));
    row.setBordered(false);
    row.setButtonType(NSButtonType::MomentaryChange);
    row.setTag(notification.key as isize);
    // SAFETY: controller lives throughout the event loop and implements rowClicked:.
    unsafe {
        row.setTarget(Some(controller));
        row.setAction(Some(sel!(rowClicked:)));
    }
    let mut title = notification.title.clone();
    if notification.count > 1 {
        title.push_str(&format!(" ×{}", notification.count));
    }
    if notification.action.is_some() {
        title.push_str(" ↗");
    }
    let text_width = width - 46.0;
    let heading = wrapping_label(
        &title,
        cfg,
        f64::from(cfg.font_size),
        &cfg.theme.title,
        2,
        text_width,
        mtm,
    );
    let title_height = heading.frame().size.height;
    heading.setFrameOrigin(NSPoint::new(30.0, 10.0));
    add(&row, &heading);
    let dot = label(
        "●",
        cfg,
        11.0,
        cfg.theme.level_color(notification.level),
        mtm,
    );
    dot.setFrame(rect(12.0, 12.0, 14.0, 18.0));
    add(&row, &dot);
    let mut height = 10.0 + title_height;
    if let Some(body) = &notification.body
        && !body.is_empty()
    {
        let body = wrapping_label(
            body,
            cfg,
            f64::from(cfg.body_font_size),
            &cfg.theme.body,
            3,
            text_width,
            mtm,
        );
        body.setFrameOrigin(NSPoint::new(30.0, height + 3.0));
        height += 3.0 + body.frame().size.height;
        add(&row, &body);
    }
    if let Some(progress) = notification.progress {
        let indicator = NSProgressIndicator::initWithFrame(
            NSProgressIndicator::alloc(mtm),
            rect(30.0, height + 6.0, text_width, 8.0),
        );
        indicator.setStyle(NSProgressIndicatorStyle::Bar);
        indicator.setIndeterminate(false);
        indicator.setMinValue(0.0);
        indicator.setMaxValue(100.0);
        indicator.setDoubleValue(f64::from(progress.min(100)));
        add(&row, &indicator);
        height += 16.0;
    }
    height = (height + 10.0).max(44.0);
    row.setFrameSize(NSSize::new(width, height));
    let mut tooltip = format!("{}\nClick to dismiss", notification.title);
    if let Some(body) = &notification.body {
        tooltip.push_str(&format!("\n{body}"));
    }
    if let Some(source) = &notification.source {
        tooltip.push_str(&format!("\nSource: {source}"));
    }
    if notification.action.is_some() {
        tooltip.push_str(" and run the configured action");
    }
    row.setToolTip(Some(&NSString::from_str(&tooltip)));
    let separator = NSView::initWithFrame(
        NSView::alloc(mtm),
        rect(14.0, height - 1.0, width - 28.0, 1.0),
    );
    set_background(&separator, &cfg.theme.separator, 0.0);
    add(&row, &separator);
    row
}

fn wrapping_label(
    text: &str,
    cfg: &Config,
    size: f64,
    color: &str,
    lines: isize,
    width: f64,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let label = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    label.setSelectable(false);
    label.setFont(Some(&font(cfg, size)));
    label.setTextColor(Some(&native_color(color)));
    label.setMaximumNumberOfLines(lines);
    label.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
    let measured = label
        .cell()
        .map(|cell| {
            cell.cellSizeForBounds(rect(0.0, 0.0, width, 10000.0))
                .height
        })
        .unwrap_or(size * 1.5);
    label.setFrame(rect(
        0.0,
        0.0,
        width,
        measured
            .ceil()
            .max(size * 1.4)
            .min(size * 1.6 * lines as f64),
    ));
    label
}

fn label(
    text: &str,
    cfg: &Config,
    size: f64,
    color: &str,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setFont(Some(&font(cfg, size)));
    label.setTextColor(Some(&native_color(color)));
    label
}

fn font(cfg: &Config, size: f64) -> Retained<NSFont> {
    NSFont::fontWithName_size(&NSString::from_str(&cfg.font), size.max(8.0))
        .unwrap_or_else(|| NSFont::systemFontOfSize(size.max(8.0)))
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}
fn contains(rect: NSRect, point: NSPoint) -> bool {
    point.x >= rect.origin.x
        && point.x < rect.origin.x + rect.size.width
        && point.y >= rect.origin.y
        && point.y < rect.origin.y + rect.size.height
}
fn intersects(a: NSRect, b: NSRect) -> bool {
    a.origin.y < b.origin.y + b.size.height && a.origin.y + a.size.height > b.origin.y
}

fn add(parent: &NSView, child: &NSView) {
    // SAFETY: every view belongs to this main-thread AppKit hierarchy.
    parent.addSubview(child);
}

fn native_color(value: &str) -> Retained<NSColor> {
    let (r, g, b, a) = parse_color(value);
    NSColor::colorWithSRGBRed_green_blue_alpha(
        f64::from(r),
        f64::from(g),
        f64::from(b),
        f64::from(a),
    )
}

fn set_background(view: &NSView, color: &str, radius: f64) {
    view.setWantsLayer(true);
    if let Some(layer) = view.layer() {
        layer.setBackgroundColor(Some(&native_color(color).CGColor()));
        layer.setCornerRadius(radius);
        layer.setMasksToBounds(radius > 0.0);
    }
}

fn screen_for(point: Point, mtm: MainThreadMarker) -> WorkArea {
    let screens = NSScreen::screens(mtm);
    let screen = screens
        .iter()
        .find(|screen| {
            let frame = screen.frame();
            WorkArea {
                x: frame.origin.x,
                y: frame.origin.y,
                width: frame.size.width,
                height: frame.size.height,
            }
            .contains(point)
        })
        .or_else(|| NSScreen::mainScreen(mtm));
    let frame = screen
        .map(|screen| screen.visibleFrame())
        .unwrap_or_else(|| rect(0.0, 0.0, 1440.0, 900.0));
    WorkArea {
        x: frame.origin.x,
        y: frame.origin.y,
        width: frame.size.width,
        height: frame.size.height,
    }
}

fn spawn_and_reap(command: &mut ProcessCommand) {
    match command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => eprintln!("could not launch command: {error}"),
    }
}

fn menu_item(controller: &Controller, title: &str, action: Sel) -> Retained<NSMenuItem> {
    // SAFETY: these selectors are implemented by Controller, which stays alive
    // until all menu objects and the run-loop timer have been removed.
    unsafe {
        let item = NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(controller.mtm()),
            &NSString::from_str(title),
            Some(action),
            ns_string!(""),
        );
        item.setTarget(Some(controller));
        item
    }
}

/// Run from the process's main thread. No notification path activates the app
/// or makes its panel a key window, so typing stays in the foreground app.
pub fn run(cfg: Config, rx: Receiver<Command>, local: Arc<PipeServer>) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("macOS UI must run on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let width = f64::from(cfg.width).clamp(240.0, 1000.0);
    let panel = BlipPanel::new(mtm, width);
    // SAFETY: the Rust retained owner controls the panel's lifetime.
    unsafe {
        panel.setReleasedWhenClosed(false);
    }
    panel.setTitle(ns_string!("Blip"));
    panel.setFloatingPanel(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setHidesOnDeactivate(false);
    panel.setLevel(NSFloatingWindowLevel);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(true);
    let root = FlippedView::new(mtm, rect(0.0, 0.0, width, 120.0));
    set_background(&root, &cfg.theme.bg, 12.0);
    panel.setContentView(Some(&root));
    // Native controls follow a dark appearance when the configured background
    // is dark; the notification text still uses the configured theme colors.
    let (r, g, b, _) = parse_color(&cfg.theme.bg);
    if r + g + b < 1.5 {
        unsafe {
            panel.setAppearance(NSAppearance::appearanceNamed(NSAppearanceNameDarkAqua).as_deref());
        }
    }
    let header = DragHandle::new(mtm, rect(0.0, 0.0, width, HEADER_HEIGHT));
    let heading = label("Blip · drag here to pin", &cfg, 11.0, &cfg.theme.body, mtm);
    heading.setFrame(rect(14.0, 9.0, width - 28.0, 18.0));
    add(&header, &heading);
    add(&root, &header);
    let scroll = NSScrollView::initWithFrame(
        NSScrollView::alloc(mtm),
        rect(0.0, HEADER_HEIGHT, width, 44.0),
    );
    scroll.setDrawsBackground(false);
    scroll.setHasVerticalScroller(true);
    scroll.setHasHorizontalScroller(false);
    scroll.setAutohidesScrollers(true);
    add(&root, &scroll);
    let footer = NSButton::initWithFrame(NSButton::alloc(mtm), rect(8.0, 83.0, width - 16.0, 30.0));
    footer.setTitle(ns_string!("Clear all"));
    footer.setBezelStyle(NSBezelStyle::Push);
    add(&root, &footer);
    let status = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let quiet_menu = NSMenuItem::new(mtm);
    let cursor = NSEvent::mouseLocation();
    let controller = Controller::new(
        mtm,
        State {
            cfg,
            rx,
            local,
            store: Store::new(),
            panel,
            root,
            header,
            scroll,
            footer,
            status,
            quiet_menu,
            rows: Vec::new(),
            sound: None,
            anchor: Point {
                x: cursor.x,
                y: cursor.y,
            },
            pinned: false,
            quiet: false,
            session_inactive: false,
            display_asleep: false,
            system_asleep: false,
            grace_until: None,
            last_tick: Instant::now(),
            quit: false,
        },
    );
    app.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Blip"));
    menu.setAutoenablesItems(false);
    for (title, selector) in [
        ("Show notifications", sel!(showPanel:)),
        ("Hide panel", sel!(hidePanel:)),
        ("Clear all", sel!(clearPanel:)),
        ("Reset position", sel!(resetPosition:)),
    ] {
        menu.addItem(&menu_item(&controller, title, selector));
    }
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let quiet = menu_item(&controller, "Quiet mode", sel!(toggleQuiet:));
    menu.addItem(&quiet);
    menu.addItem(&menu_item(
        &controller,
        "Open configuration…",
        sel!(openConfig:),
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&menu_item(&controller, "Quit Blip", sel!(quitBlip:)));
    {
        let mut state = controller.ivars().state.borrow_mut();
        state.quiet_menu = quiet;
        state.status.setMenu(Some(&menu));
        unsafe {
            state.footer.setTarget(Some(&controller));
            state.footer.setAction(Some(sel!(clearPanel:)));
        }
        state.rebuild(&controller);
    }
    let workspace = NSWorkspace::sharedWorkspace().notificationCenter();
    // SAFETY: all six selectors have the notification callback signature, and
    // observers are removed before the controller is released.
    unsafe {
        for name in [
            NSWorkspaceSessionDidResignActiveNotification,
            NSWorkspaceSessionDidBecomeActiveNotification,
            NSWorkspaceScreensDidSleepNotification,
            NSWorkspaceScreensDidWakeNotification,
            NSWorkspaceWillSleepNotification,
            NSWorkspaceDidWakeNotification,
        ] {
            workspace.addObserver_selector_name_object(
                &controller,
                sel!(workspaceChanged:),
                Some(name),
                None,
            );
        }
    }
    // The timer keeps worker callbacks independent from AppKit. Native views
    // are rebuilt only for content changes/removal, never for each TTL tick.
    let timer = unsafe {
        NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
            0.1,
            &controller,
            sel!(tick:),
            None,
            true,
        )
    };
    unsafe {
        NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
    }
    app.run();
    timer.invalidate();
    unsafe {
        workspace.removeObserver(&controller);
    }
    let state = controller.ivars().state.borrow();
    state.local.stop();
    state.panel.orderOut(None);
    NSStatusBar::systemStatusBar().removeStatusItem(&state.status);
    app.setDelegate(None);
    Ok(())
}
