//! The GPU side: D3D11 → composition swapchain → Direct2D → DirectComposition,
//! with DirectWrite doing the text.
//!
//! Two things here are worth knowing:
//!
//! * The window is created `WS_EX_NOREDIRECTIONBITMAP`, so it has no GDI
//!   surface at all. Per-pixel alpha goes straight to DWM as a premultiplied
//!   composition swapchain. That's what buys real antialiased rounded corners
//!   and a shadow that fades into whatever is behind it.
//! * The swapchain is allocated once at the panel's maximum size and is not
//!   resized for content changes. The window shrinks and grows; DComp clips the
//!   surface to it. Resizing a swapchain mid-animation flickers.

use std::collections::HashMap;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_ALIASED, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_ROUNDED_RECT, D2D1CreateFactory, ID2D1Bitmap1, ID2D1DeviceContext, ID2D1Factory1,
    ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_METRICS, DWRITE_TRIMMING,
    DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP, DWRITE_WORD_WRAPPING_WRAP,
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
    IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1,
};
use windows::core::{Interface, PCWSTR, Result};
use windows_numerics::Vector2;

use crate::config::{Config, parse_color};
use crate::model::{Level, Notification};
use crate::ui::layout::{Layout, Rect};
use crate::{wide, wide_raw};

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

fn col(hex: &str) -> D2D1_COLOR_F {
    let (r, g, b, a) = parse_color(hex);
    D2D1_COLOR_F { r, g, b, a }
}

const TRANSPARENT: D2D1_COLOR_F = D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };

pub struct Palette {
    pub bg: D2D1_COLOR_F,
    pub border: D2D1_COLOR_F,
    pub title: D2D1_COLOR_F,
    pub body: D2D1_COLOR_F,
    pub row_hover: D2D1_COLOR_F,
    pub separator: D2D1_COLOR_F,
    pub button_bg: D2D1_COLOR_F,
    pub button_hover: D2D1_COLOR_F,
    pub button_text: D2D1_COLOR_F,
    pub scrollbar: D2D1_COLOR_F,
    pub flash: D2D1_COLOR_F,
    pub level: [D2D1_COLOR_F; 3],
}

impl Palette {
    pub fn from_config(cfg: &Config) -> Self {
        let t = &cfg.theme;
        Palette {
            bg: col(&t.bg),
            border: col(&t.border),
            title: col(&t.title),
            body: col(&t.body),
            row_hover: col(&t.row_hover),
            separator: col(&t.separator),
            button_bg: col(&t.button_bg),
            button_hover: col(&t.button_hover),
            button_text: col(&t.button_text),
            scrollbar: col(&t.scrollbar),
            flash: col(&t.flash),
            level: [col(&t.level_low), col(&t.level_normal), col(&t.level_critical)],
        }
    }

    fn for_level(&self, l: Level) -> D2D1_COLOR_F {
        match l {
            Level::Low => self.level[0],
            Level::Normal => self.level[1],
            Level::Critical => self.level[2],
        }
    }
}

// ---------------------------------------------------------------------------
// Text layout cache
// ---------------------------------------------------------------------------

/// DirectWrite layouts cost tens of microseconds each to build. At 60fps with
/// six rows that adds up, and the text almost never changes between frames — so
/// key the cache on content and rebuild only when it actually moves.
struct CachedText {
    title: IDWriteTextLayout,
    body: Option<IDWriteTextLayout>,
    body_h: f32,
    sig: (String, String, u32),
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

pub struct Renderer {
    _d3d: ID3D11Device,
    swapchain: IDXGISwapChain1,
    _d2d_factory: ID2D1Factory1,
    dc: ID2D1DeviceContext,
    target_bitmap: ID2D1Bitmap1,
    dcomp: IDCompositionDevice,
    _dcomp_target: IDCompositionTarget,
    _visual: IDCompositionVisual,

    dwrite: IDWriteFactory,
    title_fmt: IDWriteTextFormat,
    body_fmt: IDWriteTextFormat,
    small_fmt: IDWriteTextFormat,

    brush: ID2D1SolidColorBrush,
    pub palette: Palette,

    cache: HashMap<u64, CachedText>,
    buf_w: u32,
    buf_h: u32,
    dpi: f32,
}

impl Renderer {
    pub fn new(hwnd: HWND, cfg: &Config, dpi: u32, max_w_dip: f32, max_h_dip: f32) -> Result<Self> {
        let dpi_f = dpi as f32;
        let scale = dpi_f / 96.0;
        let buf_w = ((max_w_dip * scale).ceil() as u32).max(16);
        let buf_h = ((max_h_dip * scale).ceil() as u32).max(16);

        // BGRA support is a hard requirement for D2D interop. Fall back to WARP
        // so the panel still appears over RDP or on a machine with a broken
        // driver — a notifier that silently fails to show up is worse than one
        // that renders on the CPU.
        let d3d =
            create_d3d(D3D_DRIVER_TYPE_HARDWARE).or_else(|_| create_d3d(D3D_DRIVER_TYPE_WARP))?;
        let dxgi_dev: IDXGIDevice = d3d.cast()?;

        let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: buf_w,
            Height: buf_h,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            // The one flag that makes the whole thing transparent.
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            Flags: 0,
        };
        let swapchain = unsafe { factory.CreateSwapChainForComposition(&dxgi_dev, &desc, None) }?;

        let d2d_factory: ID2D1Factory1 =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }?;
        let d2d_dev = unsafe { d2d_factory.CreateDevice(&dxgi_dev) }?;
        let dc = unsafe { d2d_dev.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE) }?;
        let target_bitmap = bind_backbuffer(&dc, &swapchain, dpi_f)?;

        let dcomp: IDCompositionDevice = unsafe { DCompositionCreateDevice(&dxgi_dev) }?;
        let dcomp_target = unsafe { dcomp.CreateTargetForHwnd(hwnd, true) }?;
        let visual = unsafe { dcomp.CreateVisual() }?;
        unsafe {
            visual.SetContent(&swapchain)?;
            dcomp_target.SetRoot(&visual)?;
            dcomp.Commit()?;
        }

        // The configured family is a starting point, not a mandate: DirectWrite
        // walks the system fallback chain for anything it lacks, so CJK, emoji
        // and mixed scripts all shape correctly without extra work.
        let dwrite: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }?;
        let title_fmt =
            make_format(&dwrite, &cfg.font, cfg.font_size, DWRITE_FONT_WEIGHT_SEMI_BOLD, false)?;
        let body_fmt =
            make_format(&dwrite, &cfg.font, cfg.body_font_size, DWRITE_FONT_WEIGHT_NORMAL, true)?;
        let small_fmt = make_format(
            &dwrite,
            &cfg.font,
            cfg.body_font_size - 0.5,
            DWRITE_FONT_WEIGHT_NORMAL,
            false,
        )?;

        // One brush, recoloured per draw call. Cheaper than a brush per colour
        // and keeps the palette in one place.
        let brush = unsafe { dc.CreateSolidColorBrush(&TRANSPARENT, None) }?;

        Ok(Renderer {
            _d3d: d3d,
            swapchain,
            _d2d_factory: d2d_factory,
            dc,
            target_bitmap,
            dcomp,
            _dcomp_target: dcomp_target,
            _visual: visual,
            dwrite,
            title_fmt,
            body_fmt,
            small_fmt,
            brush,
            palette: Palette::from_config(cfg),
            cache: HashMap::new(),
            buf_w,
            buf_h,
            dpi: dpi_f,
        })
    }

    pub fn dpi(&self) -> f32 {
        self.dpi
    }

    /// Grow the back buffer if the panel needs more room, or re-bind it at a new
    /// DPI. Ordinary content-driven size changes never reach here — the window
    /// shrinks and DComp clips.
    pub fn ensure(&mut self, dpi: u32, need_w_dip: f32, need_h_dip: f32) -> Result<()> {
        let dpi_f = dpi as f32;
        let scale = dpi_f / 96.0;
        let w = ((need_w_dip * scale).ceil() as u32).max(16);
        let h = ((need_h_dip * scale).ceil() as u32).max(16);

        if w <= self.buf_w && h <= self.buf_h && (dpi_f - self.dpi).abs() < 0.5 {
            return Ok(());
        }

        let new_w = w.max(self.buf_w);
        let new_h = h.max(self.buf_h);

        // The target bitmap holds a reference to the back buffer; it has to be
        // released before ResizeBuffers will succeed.
        unsafe { self.dc.SetTarget(None) };
        unsafe {
            self.swapchain.ResizeBuffers(0, new_w, new_h, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SWAP_CHAIN_FLAG(0))?;
        }
        self.target_bitmap = bind_backbuffer(&self.dc, &self.swapchain, dpi_f)?;
        self.buf_w = new_w;
        self.buf_h = new_h;
        self.dpi = dpi_f;
        // Cheap insurance against stale wrapping after a DPI move.
        self.cache.clear();
        Ok(())
    }

    /// Wrapped height of a row's body text, in DIP.
    pub fn measure_body(
        &mut self,
        n: &Notification,
        width: f32,
        max_lines: usize,
        line_h: f32,
    ) -> f32 {
        match self.text_for(n, width, max_lines, line_h) {
            Some(c) => c.body_h,
            None => 0.0,
        }
    }

    fn text_for(
        &mut self,
        n: &Notification,
        width: f32,
        max_lines: usize,
        line_h: f32,
    ) -> Option<&CachedText> {
        let body = n.body.clone().unwrap_or_default();
        let sig = (n.title.clone(), body.clone(), (width * 4.0) as u32);

        let stale = match self.cache.get(&n.key) {
            Some(c) => c.sig != sig,
            None => true,
        };

        if stale {
            let title = build_layout(&self.dwrite, &self.title_fmt, &n.title, width, line_h * 1.6)?;
            let (body_layout, body_h) = if body.is_empty() {
                (None, 0.0)
            } else {
                let max_h = line_h * max_lines as f32 + 1.0;
                let l = build_layout(&self.dwrite, &self.body_fmt, &body, width, max_h)?;
                let mut m = DWRITE_TEXT_METRICS::default();
                let h = if unsafe { l.GetMetrics(&mut m) }.is_ok() {
                    m.height.min(max_h)
                } else {
                    line_h
                };
                (Some(l), h)
            };
            self.cache.insert(n.key, CachedText { title, body: body_layout, body_h, sig });
        }
        self.cache.get(&n.key)
    }

    /// Drop cache entries for notifications that no longer exist.
    pub fn gc(&mut self, live: &[Notification]) {
        if self.cache.len() <= live.len() {
            return;
        }
        let keys: std::collections::HashSet<u64> = live.iter().map(|n| n.key).collect();
        self.cache.retain(|k, _| keys.contains(k));
    }

    // -- primitives --------------------------------------------------------

    fn fill_rr(&self, r: Rect, radius: f32, c: D2D1_COLOR_F) {
        unsafe {
            self.brush.SetColor(&c);
            self.dc.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT { rect: to_d2d(r), radiusX: radius, radiusY: radius },
                &self.brush,
            );
        }
    }

    fn fill(&self, r: Rect, c: D2D1_COLOR_F) {
        unsafe {
            self.brush.SetColor(&c);
            self.dc.FillRectangle(&to_d2d(r), &self.brush);
        }
    }

    fn stroke_rr(&self, r: Rect, radius: f32, c: D2D1_COLOR_F, w: f32) {
        unsafe {
            self.brush.SetColor(&c);
            self.dc.DrawRoundedRectangle(
                &D2D1_ROUNDED_RECT { rect: to_d2d(r), radiusX: radius, radiusY: radius },
                &self.brush,
                w,
                None,
            );
        }
    }

    /// A layered fake shadow: six expanding rounded rects at low alpha. Reads as
    /// a soft drop shadow and costs six fills. The real D2D shadow effect needs
    /// a command list and an offscreen pass for a difference nobody will notice
    /// on a 340px card.
    fn shadow(&self, card: Rect, radius: f32, strength: f32) {
        const RINGS: usize = 6;
        for i in (0..RINGS).rev() {
            let g = (i + 1) as f32 * 1.6;
            let a = 0.055 * strength * (1.0 - i as f32 / RINGS as f32);
            self.fill_rr(
                Rect::new(card.l - g, card.t - g * 0.55 + 1.5, card.r + g, card.b + g * 0.9 + 1.5),
                radius + g * 0.5,
                D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a },
            );
        }
    }

    fn draw_x(&self, r: Rect, c: D2D1_COLOR_F) {
        let pad = 6.0;
        unsafe {
            self.brush.SetColor(&c);
            self.dc.DrawLine(
                Vector2 { X: r.l + pad, Y: r.t + pad },
                Vector2 { X: r.r - pad, Y: r.b - pad },
                &self.brush,
                1.3,
                None,
            );
            self.dc.DrawLine(
                Vector2 { X: r.r - pad, Y: r.t + pad },
                Vector2 { X: r.l + pad, Y: r.b - pad },
                &self.brush,
                1.3,
                None,
            );
        }
    }

    // -- the frame ---------------------------------------------------------

    pub fn draw(&mut self, f: &Frame<'_>) -> Result<()> {
        unsafe {
            self.dc.BeginDraw();
            self.dc.Clear(Some(&TRANSPARENT));
        }

        let l = f.layout;
        let m = &l.m;
        let card = l.card;

        self.shadow(card, m.radius, f.alpha);
        self.fill_rr(card, m.radius, scale_a(self.palette.bg, f.alpha));
        self.stroke_rr(card.inset(0.5, 0.5), m.radius, scale_a(self.palette.border, f.alpha), 1.0);

        unsafe { self.dc.PushAxisAlignedClip(&to_d2d(l.viewport), D2D1_ANTIALIAS_MODE_ALIASED) };

        for g in &l.rows {
            if !l.is_on_screen(g, f.scroll) {
                continue;
            }
            let Some(n) = f.items.get(g.index) else { continue };
            let r = l.row_rect(g, f.scroll);
            let ra = f.alpha * n.appear * (1.0 - n.fade);
            if ra <= 0.01 {
                continue;
            }

            let hovered = f.hover_row == Some(g.index);
            if hovered {
                self.fill(r, scale_a(self.palette.row_hover, f.alpha));
            }

            // Flash after an in-place update, so you can tell a row changed
            // rather than having content swap silently under your eyes.
            if n.flash > 0.0 {
                self.fill(r, with_alpha(self.palette.flash, 0.16 * n.flash * f.alpha));
            }

            let lvl = self.palette.for_level(n.level);
            self.fill_rr(
                Rect::new(r.l + 1.0, r.t + 6.0, r.l + 1.0 + m.bar_w, (r.b - 6.0).max(r.t + 6.0)),
                m.bar_w * 0.5,
                scale_a(lvl, ra),
            );

            let tx = r.l + m.bar_w + m.row_pad_x;
            let mut ty = r.t + m.row_pad_y;
            let text_w = m.row_text_width();

            // End the &mut self borrow before touching the &self draw helpers.
            let (title_l, body_l) = {
                let Some(c) = self.text_for(n, text_w, m.body_max_lines, m.body_line_h) else {
                    continue;
                };
                (c.title.clone(), c.body.clone())
            };

            unsafe {
                self.brush.SetColor(&scale_a(self.palette.title, ra));
                self.dc.DrawTextLayout(
                    Vector2 { X: tx, Y: ty },
                    &title_l,
                    &self.brush,
                    D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                );
            }
            ty += m.title_line_h;

            if let Some(bl) = body_l {
                ty += m.gap;
                unsafe {
                    self.brush.SetColor(&scale_a(self.palette.body, ra));
                    self.dc.DrawTextLayout(
                        Vector2 { X: tx, Y: ty },
                        &bl,
                        &self.brush,
                        D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                    );
                }
            }

            // ×N badge for collapsed duplicates.
            if n.count > 1
                && let Some(bl) = build_layout(
                    &self.dwrite,
                    &self.small_fmt,
                    &format!("×{}", n.count),
                    60.0,
                    m.title_line_h,
                )
            {
                let br = Rect::xywh(r.r - m.close_size - 42.0, r.t + 9.0, 34.0, 17.0);
                self.fill_rr(br, 8.5, with_alpha(lvl, 0.22 * ra));
                unsafe {
                    self.brush.SetColor(&scale_a(self.palette.title, ra));
                    self.dc.DrawTextLayout(
                        Vector2 { X: br.l + 7.0, Y: br.t + 0.5 },
                        &bl,
                        &self.brush,
                        D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                    );
                }
            }

            // Per-row dismiss appears only under the pointer. A permanent column
            // of ✕ glyphs is visual noise on something you mostly just read.
            if hovered {
                let cr = l.close_rect(&r);
                if f.hover_close {
                    self.fill_rr(cr, 5.0, with_alpha(self.palette.button_hover, 0.9 * f.alpha));
                }
                self.draw_x(cr, with_alpha(self.palette.body, 0.85 * f.alpha));
            }

            if let Some(pct) = n.progress {
                let py = r.b - 4.0;
                let track_r = r.r - m.row_pad_x;
                self.fill_rr(
                    Rect::new(tx, py, track_r, py + 2.0),
                    1.0,
                    with_alpha(self.palette.separator, 0.6 * ra),
                );
                let pw = (track_r - tx) * (pct.min(100) as f32 / 100.0);
                self.fill_rr(Rect::new(tx, py, tx + pw, py + 2.0), 1.0, with_alpha(lvl, 0.95 * ra));
            }

            if g.index + 1 < l.rows.len() {
                self.fill(
                    Rect::new(r.l + m.row_pad_x, r.b - 0.5, r.r - m.row_pad_x, r.b),
                    scale_a(self.palette.separator, f.alpha),
                );
            }
        }

        unsafe { self.dc.PopAxisAlignedClip() };

        if l.scroll_max > 0.5 {
            let track = l.viewport.h();
            let thumb_h = track * (track / l.content_h).clamp(0.08, 1.0);
            let t = (f.scroll / l.scroll_max).clamp(0.0, 1.0);
            let y = l.viewport.t + t * (track - thumb_h);
            self.fill_rr(
                Rect::xywh(
                    l.viewport.r - m.scrollbar_w - 3.0,
                    y + 2.0,
                    m.scrollbar_w,
                    (thumb_h - 4.0).max(8.0),
                ),
                m.scrollbar_w * 0.5,
                scale_a(self.palette.scrollbar, f.alpha),
            );
        }

        // --- footer -----------------------------------------------------------
        self.fill(
            Rect::new(l.footer.l, l.footer.t, l.footer.r, l.footer.t + 1.0),
            scale_a(self.palette.separator, f.alpha),
        );

        let btn_c =
            if f.hover_button { self.palette.button_hover } else { self.palette.button_bg };
        self.fill_rr(l.button, 7.0, scale_a(btn_c, f.alpha));

        let label = if f.count > 1 {
            format!("都看过了 · 清空 {} 条", f.count)
        } else {
            "都看过了".to_string()
        };
        if let Some(bl) =
            build_centered(&self.dwrite, &self.small_fmt, &label, l.button.w(), l.button.h())
        {
            unsafe {
                self.brush.SetColor(&scale_a(self.palette.button_text, f.alpha));
                self.dc.DrawTextLayout(
                    Vector2 {
                        X: l.button.l,
                        Y: l.button.t + (l.button.h() - m.body_line_h) * 0.5,
                    },
                    &bl,
                    &self.brush,
                    D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                );
            }
        }

        unsafe { self.dc.EndDraw(None, None) }?;
        unsafe { self.swapchain.Present(1, DXGI_PRESENT(0)).ok() }?;
        unsafe { self.dcomp.Commit() }?;
        Ok(())
    }
}

/// Everything the renderer needs for one frame, as a single struct so the draw
/// call doesn't grow a dozen positional arguments.
pub struct Frame<'a> {
    pub layout: &'a Layout,
    pub items: &'a [Notification],
    pub scroll: f32,
    pub hover_row: Option<usize>,
    pub hover_close: bool,
    pub hover_button: bool,
    /// Whole-panel fade, 0..1.
    pub alpha: f32,
    pub count: usize,
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn create_d3d(kind: D3D_DRIVER_TYPE) -> Result<ID3D11Device> {
    let mut dev: Option<ID3D11Device> = None;
    unsafe {
        D3D11CreateDevice(
            None,
            kind,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            None,
        )
    }?;
    dev.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))
}

fn bind_backbuffer(
    dc: &ID2D1DeviceContext,
    swapchain: &IDXGISwapChain1,
    dpi: f32,
) -> Result<ID2D1Bitmap1> {
    let back: IDXGISurface = unsafe { swapchain.GetBuffer(0) }?;
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        // Setting DPI on the target is what makes every coordinate above this
        // point a DIP. Nothing in layout or draw multiplies by a scale factor.
        dpiX: dpi,
        dpiY: dpi,
        bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
        colorContext: std::mem::ManuallyDrop::new(None),
    };
    let bmp = unsafe { dc.CreateBitmapFromDxgiSurface(&back, Some(&props)) }?;
    unsafe {
        dc.SetTarget(&bmp);
        dc.SetDpi(dpi, dpi);
    }
    Ok(bmp)
}

fn make_format(
    dwrite: &IDWriteFactory,
    family: &str,
    size: f32,
    weight: DWRITE_FONT_WEIGHT,
    wrap: bool,
) -> Result<IDWriteTextFormat> {
    let fam = wide(family);
    let loc = wide("zh-cn");
    let fmt = unsafe {
        dwrite.CreateTextFormat(
            PCWSTR(fam.as_ptr()),
            None,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            PCWSTR(loc.as_ptr()),
        )
    }?;
    unsafe {
        fmt.SetWordWrapping(if wrap {
            DWRITE_WORD_WRAPPING_WRAP
        } else {
            DWRITE_WORD_WRAPPING_NO_WRAP
        })?;
        // Elide overflow with a real ellipsis rather than a hard clip, so a long
        // title reads as truncated instead of broken.
        let sign = dwrite.CreateEllipsisTrimmingSign(&fmt)?;
        let trim = DWRITE_TRIMMING {
            granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
            delimiter: 0,
            delimiterCount: 0,
        };
        fmt.SetTrimming(&trim, &sign)?;
    }
    Ok(fmt)
}

fn build_layout(
    dwrite: &IDWriteFactory,
    fmt: &IDWriteTextFormat,
    text: &str,
    w: f32,
    h: f32,
) -> Option<IDWriteTextLayout> {
    let u = wide_raw(text);
    unsafe { dwrite.CreateTextLayout(&u, fmt, w.max(1.0), h.max(1.0)) }.ok()
}

fn build_centered(
    dwrite: &IDWriteFactory,
    fmt: &IDWriteTextFormat,
    text: &str,
    w: f32,
    h: f32,
) -> Option<IDWriteTextLayout> {
    let l = build_layout(dwrite, fmt, text, w, h)?;
    unsafe { l.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER) }.ok()?;
    Some(l)
}

fn to_d2d(r: Rect) -> D2D_RECT_F {
    D2D_RECT_F { left: r.l, top: r.t, right: r.r, bottom: r.b }
}

fn with_alpha(c: D2D1_COLOR_F, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { a: a.clamp(0.0, 1.0), ..c }
}

/// Multiply a palette colour's own alpha by a fade factor.
fn scale_a(c: D2D1_COLOR_F, k: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { a: (c.a * k).clamp(0.0, 1.0), ..c }
}
