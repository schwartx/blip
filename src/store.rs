//! The notification list and its lifetime rules.
//!
//! The important decision lives in [`Store::tick`]: **a row's TTL only runs
//! while that row is actually on screen and unattended.** If the panel is
//! hidden, or the row is scrolled out of view, or the mouse is resting on the
//! panel, or the session is locked, the countdown pauses.
//!
//! The alternative — wall-clock expiry — means walking away from the machine
//! silently discards everything, which destroys the one property a notifier has
//! to have: if it told you something, you got to see it.

use crate::config::Config;
use crate::model::{Level, Notification, NotifyRequest};

/// What the daemon should do as a result of an arrival.
#[derive(Debug, Default, Clone, Copy)]
pub struct Arrival {
    /// Pull the panel open (or keep it open).
    pub pop: bool,
    /// Play the level's sound.
    pub sound: bool,
    /// True when this replaced an existing row rather than adding one.
    pub updated: bool,
    pub level: Level,
}

pub struct Store {
    pub items: Vec<Notification>,
    next_key: u64,
    /// Seconds since the last time each id caused the panel to pop, used to
    /// debounce progress streams.
    last_pop: Vec<(String, f32)>,
    clock: f32,
}

impl Store {
    pub fn new() -> Self {
        Store { items: Vec::new(), next_key: 1, last_pop: Vec::new(), clock: 0.0 }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Rows that haven't started fading out. The tray badge counts these.
    pub fn live_count(&self) -> usize {
        self.items.iter().filter(|n| !n.dying).count()
    }

    pub fn push(&mut self, req: NotifyRequest, cfg: &Config) -> Arrival {
        let level = req.level.unwrap_or_default();
        let ttl = req.ttl.unwrap_or_else(|| cfg.levels.ttl_for(level));
        let mut out = Arrival { level, ..Default::default() };

        // --- same id: replace in place ---------------------------------------
        if let Some(id) = req.id.as_deref()
            && let Some(n) = self.items.iter_mut().find(|n| n.id.as_deref() == Some(id) && !n.dying)
        {
            n.title = req.title;
            n.body = req.body;
            n.level = level;
            n.source = req.source.or(n.source.take());
            n.action = req.action.or(n.action.take());
            n.progress = req.progress;
            n.ttl = ttl;
            n.remaining = ttl;
            n.flash = 1.0;
            n.measured = None;
            out.updated = true;

            // A progress stream shouldn't yank the panel open on every percent.
            let recent = self.recent_pop(id, cfg.behavior.repop_debounce);
            out.pop = cfg.levels.pops_for(level) && !recent;
            out.sound = out.pop && req.progress.is_none();
            if out.pop {
                self.mark_pop(id);
            }
            return out;
        }

        // --- identical content: collapse into a ×N badge ----------------------
        if req.id.is_none()
            && let Some(n) = self.items.iter_mut().find(|n| {
                !n.dying
                    && n.title == req.title
                    && n.body == req.body
                    && n.source.as_deref() == req.source.as_deref()
            })
        {
            n.count += 1;
            n.remaining = ttl;
            n.flash = 1.0;
            out.updated = true;
            out.pop = cfg.levels.pops_for(level);
            out.sound = false; // the first one already made a noise
            return out;
        }

        // --- genuinely new ----------------------------------------------------
        let key = self.next_key;
        self.next_key += 1;
        self.items.push(Notification {
            key,
            id: req.id.clone(),
            title: req.title,
            body: req.body,
            level,
            source: req.source,
            action: req.action,
            progress: req.progress,
            count: 1,
            ttl,
            remaining: ttl,
            flash: 0.0,
            appear: 0.0,
            dying: false,
            fade: 0.0,
            measured: None,
        });

        // Evict oldest beyond the cap. This is a notifier, not an archive.
        while self.items.len() > cfg.max_items {
            self.items.remove(0);
        }

        out.pop = cfg.levels.pops_for(level);
        out.sound = true;
        if let Some(id) = req.id.as_deref() {
            self.mark_pop(id);
        }
        out
    }

    /// Begin the fade-out. Rows are never yanked from under the cursor — they
    /// animate out so the list doesn't jump while you're reading it.
    pub fn dismiss_key(&mut self, key: u64) {
        if let Some(n) = self.items.iter_mut().find(|n| n.key == key) {
            n.dying = true;
        }
    }

    pub fn dismiss_id(&mut self, id: &str) {
        for n in self.items.iter_mut().filter(|n| n.id.as_deref() == Some(id)) {
            n.dying = true;
        }
    }

    /// The "seen everything" action. Immediate, no fade — the panel is going
    /// away in the same frame anyway.
    pub fn clear(&mut self) {
        self.items.clear();
        self.last_pop.clear();
    }

    pub fn index_of(&self, key: u64) -> Option<usize> {
        self.items.iter().position(|n| n.key == key)
    }

    /// Advance all per-row animation and expiry.
    ///
    /// `on_screen` reports whether row `i` is currently within the scrolled
    /// viewport — rows you can't see don't burn their TTL.
    ///
    /// Returns `true` if anything changed and the panel needs a redraw.
    pub fn tick(&mut self, dt: f32, ctx: &TickCtx, on_screen: impl Fn(usize) -> bool) -> bool {
        self.clock += dt;
        for (_, t) in self.last_pop.iter_mut() {
            *t += dt;
        }
        self.last_pop.retain(|(_, t)| *t < 30.0);

        let mut dirty = false;
        let anim = ctx.anim.max(0.01);

        for i in 0..self.items.len() {
            let visible = on_screen(i);
            let n = &mut self.items[i];

            if n.appear < 1.0 {
                n.appear = (n.appear + dt / anim).min(1.0);
                dirty = true;
            }
            if n.flash > 0.0 {
                n.flash = (n.flash - dt / 0.6).max(0.0);
                dirty = true;
            }

            if n.dying {
                n.fade = (n.fade + dt / anim).min(1.0);
                dirty = true;
                continue;
            }

            // The whole point: only count down what the user can actually see.
            let counting = ctx.panel_visible
                && visible
                && !ctx.hovered
                && !ctx.session_locked
                && !n.is_sticky();

            if counting {
                n.remaining -= dt;
                if n.remaining <= 0.0 {
                    n.dying = true;
                }
                dirty = true;
            }
        }

        let before = self.items.len();
        self.items.retain(|n| !(n.dying && n.fade >= 1.0));
        dirty |= self.items.len() != before;
        dirty
    }

    fn recent_pop(&self, id: &str, window: f32) -> bool {
        self.last_pop.iter().any(|(k, t)| k == id && *t < window)
    }

    fn mark_pop(&mut self, id: &str) {
        if let Some(e) = self.last_pop.iter_mut().find(|(k, _)| k == id) {
            e.1 = 0.0;
        } else {
            self.last_pop.push((id.to_string(), 0.0));
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TickCtx {
    pub panel_visible: bool,
    /// Mouse is resting on the panel — you're reading, so nothing expires.
    pub hovered: bool,
    pub session_locked: bool,
    pub anim: f32,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(title: &str) -> NotifyRequest {
        NotifyRequest { title: title.into(), ..Default::default() }
    }

    fn ctx() -> TickCtx {
        TickCtx { panel_visible: true, hovered: false, session_locked: false, anim: 0.18 }
    }

    #[test]
    fn same_id_updates_in_place() {
        let cfg = Config::default();
        let mut s = Store::new();
        let mut a = req("编译中 20%");
        a.id = Some("build".into());
        s.push(a, &cfg);

        let mut b = req("编译中 60%");
        b.id = Some("build".into());
        let out = s.push(b, &cfg);

        assert!(out.updated);
        assert_eq!(s.len(), 1, "same id must not add a second row");
        assert_eq!(s.items[0].title, "编译中 60%");
        assert_eq!(s.items[0].flash, 1.0, "an updated row should flash");
    }

    #[test]
    fn identical_content_collapses_to_count() {
        let cfg = Config::default();
        let mut s = Store::new();
        s.push(req("测试失败"), &cfg);
        s.push(req("测试失败"), &cfg);
        s.push(req("测试失败"), &cfg);
        assert_eq!(s.len(), 1);
        assert_eq!(s.items[0].count, 3);
    }

    #[test]
    fn ttl_pauses_while_panel_hidden() {
        let cfg = Config::default();
        let mut s = Store::new();
        s.push(req("hi"), &cfg);
        let start = s.items[0].remaining;

        let mut c = ctx();
        c.panel_visible = false;
        for _ in 0..600 {
            s.tick(0.016, &c, |_| true);
        }
        assert_eq!(s.len(), 1, "hidden panel must not expire anything");
        assert_eq!(s.items[0].remaining, start);
    }

    #[test]
    fn ttl_pauses_while_hovered() {
        let cfg = Config::default();
        let mut s = Store::new();
        s.push(req("hi"), &cfg);
        let mut c = ctx();
        c.hovered = true;
        for _ in 0..600 {
            s.tick(0.016, &c, |_| true);
        }
        assert_eq!(s.len(), 1, "hovering means you're reading it");
    }

    #[test]
    fn ttl_pauses_when_scrolled_out_of_view() {
        let cfg = Config::default();
        let mut s = Store::new();
        s.push(req("hi"), &cfg);
        for _ in 0..600 {
            s.tick(0.016, &ctx(), |_| false); // never on screen
        }
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn visible_row_expires_and_is_removed() {
        let cfg = Config::default();
        let mut s = Store::new();
        s.push(req("hi"), &cfg);
        for _ in 0..1000 {
            s.tick(0.016, &ctx(), |_| true);
        }
        assert_eq!(s.len(), 0, "a seen row should expire and be dropped");
    }

    #[test]
    fn critical_never_expires() {
        let cfg = Config::default();
        let mut s = Store::new();
        let mut r = req("生产环境挂了");
        r.level = Some(Level::Critical);
        s.push(r, &cfg);
        for _ in 0..2000 {
            s.tick(0.016, &ctx(), |_| true);
        }
        assert_eq!(s.len(), 1, "critical must be cleared by hand");
    }

    #[test]
    fn cap_evicts_oldest() {
        let cfg = Config { max_items: 3, ..Default::default() };
        let mut s = Store::new();
        for i in 0..6 {
            s.push(req(&format!("n{i}")), &cfg);
        }
        assert_eq!(s.len(), 3);
        assert_eq!(s.items[0].title, "n3");
    }

    #[test]
    fn progress_updates_do_not_repop() {
        let cfg = Config::default();
        let mut s = Store::new();
        let mut a = req("编译中");
        a.id = Some("b".into());
        a.progress = Some(10);
        assert!(s.push(a, &cfg).pop, "first arrival should open the panel");

        let mut b = req("编译中");
        b.id = Some("b".into());
        b.progress = Some(20);
        assert!(!s.push(b, &cfg).pop, "a progress tick should not re-yank it open");
    }
}
