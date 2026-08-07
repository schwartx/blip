//! Geometry. Everything here is in DIP; DPI scaling happens once, at the
//! Direct2D device context, so no code below this line ever multiplies by a
//! scale factor.
//!
//! The renderer and the hit-tester both consume the same `Layout`, which is the
//! only way to guarantee that what you see is what you can click.

use crate::model::Notification;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl Rect {
    pub fn new(l: f32, t: f32, r: f32, b: f32) -> Self {
        Rect { l, t, r, b }
    }
    pub fn xywh(x: f32, y: f32, w: f32, h: f32) -> Self {
        Rect { l: x, t: y, r: x + w, b: y + h }
    }
    pub fn w(&self) -> f32 {
        self.r - self.l
    }
    pub fn h(&self) -> f32 {
        self.b - self.t
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }
    pub fn inset(&self, dx: f32, dy: f32) -> Rect {
        Rect { l: self.l + dx, t: self.t + dy, r: self.r - dx, b: self.b - dy }
    }
    pub fn offset(&self, dx: f32, dy: f32) -> Rect {
        Rect { l: self.l + dx, t: self.t + dy, r: self.r + dx, b: self.b + dy }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    /// Card width, excluding the shadow gutter.
    pub width: f32,
    /// Transparent gutter around the card where the drop shadow lives. The
    /// window is bigger than the visible card by this much on every side.
    pub pad: f32,
    pub radius: f32,

    pub row_pad_x: f32,
    pub row_pad_y: f32,
    pub row_min_h: f32,
    /// Width of the coloured level stripe down the left edge of a row.
    pub bar_w: f32,
    pub title_line_h: f32,
    pub body_line_h: f32,
    /// Body text is clamped to this many lines; the rest is elided.
    pub body_max_lines: usize,
    pub gap: f32,

    pub footer_h: f32,
    pub scrollbar_w: f32,
    /// Extra height reserved on rows that carry a progress bar.
    pub progress_h: f32,

    pub max_visible_rows: usize,
}

impl Metrics {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Metrics {
            width: cfg.width,
            pad: 12.0,
            radius: 12.0,
            row_pad_x: 14.0,
            row_pad_y: 9.0,
            row_min_h: 42.0,
            bar_w: 3.0,
            title_line_h: cfg.font_size * 1.45,
            body_line_h: cfg.body_font_size * 1.4,
            body_max_lines: 3,
            gap: 3.0,
            footer_h: 34.0,
            scrollbar_w: 3.0,
            progress_h: 7.0,
            max_visible_rows: cfg.max_visible_rows.max(1),
        }
    }

    /// Text width available inside a row, after the stripe and the padding.
    ///
    /// Nothing is reserved on the right any more: with the per-row ✕ gone, the
    /// full width belongs to the text.
    pub fn row_text_width(&self) -> f32 {
        self.width - self.bar_w - self.row_pad_x * 2.0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RowGeom {
    pub index: usize,
    pub key: u64,
    /// Y within list content space (before scroll is applied).
    pub y: f32,
    pub h: f32,
}

#[derive(Clone, Debug)]
pub struct Layout {
    pub m: Metrics,
    pub rows: Vec<RowGeom>,
    /// Total height of all rows.
    pub content_h: f32,
    /// The card, in window coordinates (i.e. offset by `pad`).
    pub card: Rect,
    /// Clipping region for the scrolling list.
    pub viewport: Rect,
    pub footer: Rect,
    pub button: Rect,
    /// Full window size including the shadow gutter.
    pub window_w: f32,
    pub window_h: f32,
    pub scroll_max: f32,
}

impl Layout {
    /// `measure` returns the wrapped body height in DIP for a row; the renderer
    /// supplies it via DirectWrite and caches the result on the notification.
    pub fn build(items: &[Notification], m: &Metrics, measure: impl Fn(usize) -> f32) -> Layout {
        let mut rows = Vec::with_capacity(items.len());
        let mut y = 0.0f32;

        for (i, n) in items.iter().enumerate() {
            let body_h = if n.body.is_some() { measure(i) } else { 0.0 };
            let mut h = m.row_pad_y * 2.0 + m.title_line_h;
            if body_h > 0.0 {
                h += m.gap + body_h;
            }
            // The progress bar hangs off the bottom edge of the row; without
            // this it collides with the body text and the separator below.
            if n.progress.is_some() {
                h += m.progress_h;
            }
            h = h.max(m.row_min_h);

            // A dying row collapses as it fades so the list closes the gap
            // instead of leaving a hole.
            if n.dying {
                h *= 1.0 - n.fade;
            }

            rows.push(RowGeom { index: i, key: n.key, y, h });
            y += h;
        }

        let content_h = y;

        // Grow to fit, up to the cap; beyond that the list scrolls.
        let cap = m.row_min_h * m.max_visible_rows as f32 + m.title_line_h * 0.6;
        let list_h = content_h.min(cap.max(m.row_min_h));

        let card = Rect::xywh(m.pad, m.pad, m.width, list_h + m.footer_h);
        let viewport = Rect::new(card.l, card.t, card.r, card.t + list_h);
        let footer = Rect::new(card.l, viewport.b, card.r, card.b);

        // The dismiss-all control is deliberately huge: it is the single most
        // used action, and making the user aim at a 12px glyph to get rid of
        // something they've already read is the exact failure we're fixing.
        let button = footer.inset(6.0, 5.0);

        Layout {
            m: *m,
            rows,
            content_h,
            card,
            viewport,
            footer,
            button,
            window_w: m.width + m.pad * 2.0,
            window_h: card.h() + m.pad * 2.0,
            scroll_max: (content_h - viewport.h()).max(0.0),
        }
    }

    /// Row rect in window coordinates for the current scroll offset.
    pub fn row_rect(&self, g: &RowGeom, scroll: f32) -> Rect {
        Rect::new(
            self.viewport.l,
            self.viewport.t + g.y - scroll,
            self.viewport.r,
            self.viewport.t + g.y - scroll + g.h,
        )
    }

    pub fn is_on_screen(&self, g: &RowGeom, scroll: f32) -> bool {
        let top = g.y - scroll;
        let bottom = top + g.h;
        bottom > 0.0 && top < self.viewport.h()
    }

    pub fn hit(&self, x: f32, y: f32, scroll: f32) -> Hit {
        if self.button.contains(x, y) {
            return Hit::DismissAll;
        }
        if self.viewport.contains(x, y) {
            for g in &self.rows {
                let r = self.row_rect(g, scroll);
                if r.contains(x, y) {
                    return Hit::Row(g.index, g.key);
                }
            }
            return Hit::Background;
        }
        if self.card.contains(x, y) {
            return Hit::Background;
        }
        Hit::Outside
    }
}

/// What sits under the pointer. `Background` is draggable; rows and the button
/// are not, unless the pointer moves past the drag threshold first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Outside,
    Background,
    Row(usize, u64),
    DismissAll,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::{Level, Notification};

    fn note(key: u64, body: bool) -> Notification {
        Notification {
            key,
            id: None,
            title: "t".into(),
            body: body.then(|| "b".to_string()),
            level: Level::Normal,
            source: None,
            action: None,
            progress: None,
            count: 1,
            ttl: 5.0,
            remaining: 5.0,
            flash: 0.0,
            appear: 1.0,
            dying: false,
            fade: 0.0,
            measured: None,
        }
    }

    #[test]
    fn panel_grows_with_content_then_caps() {
        let cfg = Config::default();
        let m = Metrics::from_config(&cfg);

        let one = Layout::build(&[note(1, false)], &m, |_| 0.0);
        let many: Vec<_> = (0..20).map(|i| note(i, false)).collect();
        let lots = Layout::build(&many, &m, |_| 0.0);

        assert!(one.window_h < lots.window_h, "panel should grow with content");
        assert!(lots.scroll_max > 0.0, "past the cap it must scroll");
        assert!(
            lots.window_h < m.row_min_h * 20.0,
            "panel must not grow unbounded with 20 items"
        );
    }

    #[test]
    fn hit_test_matches_drawn_rows() {
        let cfg = Config::default();
        let m = Metrics::from_config(&cfg);
        let items: Vec<_> = (0..3).map(|i| note(i, false)).collect();
        let l = Layout::build(&items, &m, |_| 0.0);

        for g in &l.rows {
            let r = l.row_rect(g, 0.0);
            // A point just inside the row, left of the close button.
            let hit = l.hit(r.l + 20.0, r.t + r.h() / 2.0, 0.0);
            assert_eq!(hit, Hit::Row(g.index, g.key));
        }
    }

    #[test]
    fn dismiss_all_button_is_a_big_target() {
        let cfg = Config::default();
        let m = Metrics::from_config(&cfg);
        let l = Layout::build(&[note(1, false)], &m, |_| 0.0);
        assert!(l.button.w() > 200.0, "the primary action must be easy to hit");
        assert!(l.button.h() >= 22.0);
    }

    #[test]
    fn scrolled_out_rows_report_off_screen() {
        let cfg = Config::default();
        let m = Metrics::from_config(&cfg);
        let items: Vec<_> = (0..20).map(|i| note(i, false)).collect();
        let l = Layout::build(&items, &m, |_| 0.0);
        assert!(l.is_on_screen(&l.rows[0], 0.0));
        assert!(!l.is_on_screen(&l.rows[19], 0.0), "row 19 is far below the fold");
    }
}
