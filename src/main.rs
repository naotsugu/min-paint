#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{PathBuf};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt as _;
use gpui_kit::*;
use gpui_kit::assets::{AllAssets, IconName};
use gpui_kit::component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_kit::component::input::{Input, InputState, NumberInput};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::button::{Button, ButtonVariants as _, Toggle};
use gpui_kit::component::{ActiveTheme, Colorize, Icon, IndexPath, Sizable};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::Theme;

use rust_i18n::t;
rust_i18n::i18n!("locales", fallback = "en");

mod model;
use model::{
    is_valid_canvas_size, over, save_format, Document, ImageError, Painter, Pixmap, SizeError,
    TextRenderer, Tool, BACKGROUND_WHITE, MAX_CANVAS_SIZE,
};

mod mcp;
use mcp::{CanvasInfo, McpOp, McpReply, McpRequest, Style};

// Windows-only native clipboard access (CF_DIB / CF_DIBV5 / "PNG")
#[cfg(windows)]
mod win_clipboard;

// palette (column-major pairs: top row, bottom row)
const PALETTE: [u32; 20] = [
    0x000000, 0xffffff, // black, white
    0x7f7f7f, 0xc3c3c3, // dark gray, light gray
    0x880015, 0xb97a57, // dark red, brown
    0xed1c24, 0xffaec9, // red, rose
    0xff7f27, 0xffc90e, // orange, gold
    0xfff200, 0xefe4b0, // yellow, light yellow
    0x22b14c, 0xb5e61d, // green, lime
    0x00a2e8, 0x99d9ea, // turquoise, light turquoise
    0x3f48cc, 0x7092be, // indigo, blue-gray
    0xa349a4, 0xc8bfe7, // purple, lavender
];

// UI size
const SIDEBAR_WIDTH: f32 = 72.;
const SWATCH_SIZE: f32 = 16.;
const FOOTER_HEIGHT: f32 = 32.;
const SCROLLBAR_SIZE: f32 = 12.;
const MIN_THUMB_LEN: f32 = 24.;

// minimum gap between the canvas and the viewport edge when scrolled to an end
const CANVAS_MARGIN: f32 = 12.;

const MIN_ZOOM_PERCENT: f32 = 25.;
const MAX_ZOOM_PERCENT: f32 = 1600.;
// the pixel grid is shown from this zoom factor (8.0 = 800%)
const GRID_MIN_ZOOM: f32 = 8.;

const DEFAULT_CANVAS_SIZE: (usize, usize) = (600, 500);

// checkerboard shown behind transparent pixels
const CHECKER_CELL: f32 = 8.; // logical pixels
const CHECKER_LIGHT: [u8; 3] = [0xff, 0xff, 0xff];
const CHECKER_DARK: [u8; 3] = [0xcc, 0xcc, 0xcc];

/// What the current mouse drag is doing.
#[derive(Clone, Copy)]
enum Drag {
    None,
    Draw,
    Shape { start: (i32, i32) },
    SelectNew { start: (i32, i32) },
    SelectMove { grab: (i32, i32) },
    // rectangle drag of the mosaic tool; the rectangle is shown through `selection`
    MosaicNew { start: (i32, i32) },
    // `grab` is the offset (in logical pixels) between the pointer and the thumb start
    ScrollbarX { grab: f32 },
    ScrollbarY { grab: f32 },
    // right-button drag scrolls the viewport;
    // `last` is the previous pointer position in window coordinates (logical pixels)
    Pan { last: (f32, f32) },
}

/// Geometry of one scrollbar thumb, in logical pixels.
#[derive(Clone, Copy)]
struct Thumb {
    start: f32,  // thumb offset inside the track
    len: f32,    // thumb length
    track: f32,  // track length
    max: f32,    // maximum scroll offset of this axis
}

fn thumb(content: f32, view: f32, scroll: f32, track: f32) -> Option<Thumb> {
    let max = content - view;
    if max <= 0.5 || track <= 0. {
        return None;
    }
    let len = (track * view / content).clamp(MIN_THUMB_LEN.min(track), track);
    let start = scroll / max * (track - len);
    Some(Thumb { start, len, track, max })
}

#[derive(Clone, Debug)]
enum DiscardAction {
    NewDocument,
    LoadImage(PathBuf),
}

pub struct Application {
    doc: Document,
    painter: Painter,
    text: TextRenderer,
    // true while an async save dialog is pending
    is_saving: bool,
    drag: Drag,
    // last pixel touched while dragging (canvas coordinates)
    last_pos: (i32, i32),
    // whether the mouse pointer is over the selection (decides the pointer shape)
    over_selection: bool,
    // pixel under the mouse pointer, if it is inside the canvas
    cursor_px: Option<(i32, i32)>,
    zoom_percent: f32,
    // how far the viewport is scrolled into the content, in logical pixels (>= 0)
    scroll: (f32, f32),
    size_select: Entity<SelectState<Vec<&'static str>>>,
    width_input: Entity<InputState>,
    height_input: Entity<InputState>,
    color_picker: Entity<ColorPickerState>,
    zoom_slider: Entity<SliderState>,
    // size of the viewport area (window minus sidebar and footer), updated every render
    viewport_size: Size<Pixels>,
    // viewport origin in window coordinates, recorded every frame during prepaint
    viewport_origin: Rc<Cell<Point<Pixels>>>,
    // the visible part of the pixel map, already magnified (device pixel resolution)
    view_image: Option<Arc<RenderImage>>,
    view_pixels: (usize, usize),
    view_dirty: bool,
    // reusable coordinate mapping caches for view rendering
    xs_cache: Vec<i32>,
    ys_cache: Vec<i32>,
    focus_handle: FocusHandle,
    // keep subscriptions alive for the lifetime of the view
    _subscriptions: Vec<Subscription>,
}

impl Application {
    fn new(
        initial_file: Option<PathBuf>,
        open_rx: Option<UnboundedReceiver<PathBuf>>,
        mcp_rx: Option<UnboundedReceiver<McpRequest>>,
        window: &mut Window,
        cx: &mut Context<Self>) -> Self
    {
        // brush size select
        let size_select = cx.new(|cx| {
            SelectState::new(
                vec!["1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "12", "16", "24", "32", "48", "64"],
                Some(IndexPath::new(2)),
                window,
                cx,
            )
        });

        // reflect the select into the brush size
        let size_sub = cx.subscribe_in(&size_select, window, |this, _, ev, _, cx| {
                if let SelectEvent::Confirm(Some(val)) = ev {
                    if let Ok(v) = val.parse::<i32>() {
                        this.painter.brush_size = v.clamp(1, 500);
                        cx.notify();
                    }
                }
            },
        );

        // canvas size inputs (applied with the Resize button)
        let width_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(DEFAULT_CANVAS_SIZE.0.to_string())
                .step(10.)
                .min(1.)
        });
        let height_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(DEFAULT_CANVAS_SIZE.1.to_string())
                .step(10.)
                .min(1.)
        });

        let initial_color = hex_to_rgba(PALETTE[0]);
        let color_picker = cx.new(|cx| ColorPickerState::new(window, cx).default_value(rgba_to_hsla(initial_color)));

        // reflect colors chosen in the picker popover into the drawing color
        let color_sub = cx.subscribe(&color_picker, |this, _, ev, cx| match ev {
            ColorPickerEvent::Change(color) => {
                if let Some(color) = color {
                    this.painter.color = to_rgba8(*color);
                    cx.notify();
                }
            }
        });

        // zoom slider (percent)
        let zoom_slider = cx.new(|_| {
            SliderState::new()
                .min(MIN_ZOOM_PERCENT)
                .max(MAX_ZOOM_PERCENT)
                .step(25.)
                .default_value(100.)
        });

        // reflect the slider into the zoom factor, keeping the viewport center fixed
        let zoom_sub = cx.subscribe(&zoom_slider, |this, _, ev: &SliderEvent, cx| {
            if let SliderEvent::Change(value) = ev {
                this.apply_zoom(value.start());
                cx.notify();
            }
        });

        // follow the OS appearance (light / dark): apply it now and whenever it changes
        Theme::sync_system_appearance(Some(window), cx);
        update_theme_color(cx);
        let appearance_sub = window.observe_window_appearance(|window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
            update_theme_color(cx);
            window.refresh();
        });

        // ask about unsaved changes when the window is about to close (X button, Alt+F4, ...);
        // returning false from the callback keeps the window open
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_window, cx| {
            this.update(cx, |app, cx| app.handle_window_close(cx)).unwrap_or(true)
        });

        // Quit (menu / Cmd+Q): ask about unsaved changes the same way as closing the window.
        // `Context::on_action` shadows `App::on_action`, so call the App one explicitly.
        let weak = cx.entity().downgrade();
        App::on_action(cx, move |_: &Quit, cx| {
            weak.update(cx, |app, cx| app.request_quit(cx)).ok();
        });

        let mut app = Self {
            doc: Document::new(DEFAULT_CANVAS_SIZE.0, DEFAULT_CANVAS_SIZE.1, BACKGROUND_WHITE),
            painter: Painter::new(initial_color),
            text: TextRenderer::default(),
            is_saving: false,
            drag: Drag::None,
            last_pos: (0, 0),
            over_selection: false,
            cursor_px: None,
            zoom_percent: 100.,
            scroll: (0., 0.),
            size_select,
            width_input,
            height_input,
            color_picker,
            zoom_slider,
            viewport_size: Size::default(),
            viewport_origin: Rc::new(Cell::new(Point::default())),
            view_image: None,
            view_pixels: (0, 0),
            view_dirty: true,
            xs_cache: Vec::new(),
            ys_cache: Vec::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: vec![color_sub, size_sub, zoom_sub, appearance_sub],
        };
        // give the canvas the keyboard focus at startup so that the shortcuts work at once
        window.focus(&app.focus_handle, cx);

        // open the file given on the command line (a fresh document has no changes,
        // so no confirmation dialog appears)
        if let Some(path) = initial_file {
            app.perform_discard_action(DiscardAction::LoadImage(path), window, cx);
        }

        // files requested by the OS after startup (Finder "Open With", drop on the Dock icon, ...)
        if let Some(mut rx) = open_rx {
            cx.spawn(async move |this, cx| {
                while let Some(path) = rx.next().await {
                    let alive = this.update_in(cx, |this, window, cx| {
                        window.activate_window();
                        // asks about unsaved changes first, like the Open dialog does
                        this.check_discard_and_run(DiscardAction::LoadImage(path), window, cx);
                    });
                    if alive.is_err() {
                        break; // the view is gone
                    }
                }
            })
            .detach();
        }

        // requests from the MCP server (opt-in with --mcp-port)
        if let Some(mut rx) = mcp_rx {
            cx.spawn(async move |this, cx| {
                while let Some(req) = rx.next().await {
                    let alive = this.update_in(cx, |this, window, cx| this.handle_mcp(req, window, cx));
                    if alive.is_err() {
                        break; // the view is gone
                    }
                }
            })
            .detach();
        }

        app
    }

    // zoom factor (1.0 = 100%)
    fn zoom(&self) -> f32 {
        self.zoom_percent / 100.
    }

    fn mark_dirty(&mut self) {
        self.view_dirty = true;
    }

    // move the keyboard focus to the canvas so that the shortcuts work
    fn focus_canvas(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
    }

    // -- view geometry -------------------------------------------------------

    // size of the scrollable content at the given zoom: the canvas plus margins, but never
    // smaller than the viewport (so the canvas can be centered when it is small)
    fn content_size(&self, zoom: f32) -> (f32, f32) {
        let view_w = f32::from(self.viewport_size.width);
        let view_h = f32::from(self.viewport_size.height);
        (
            (self.doc.pixmap.width as f32 * zoom + CANVAS_MARGIN * 2.).max(view_w),
            (self.doc.pixmap.height as f32 * zoom + CANVAS_MARGIN * 2.).max(view_h),
        )
    }

    fn clamp_scroll(&mut self) {
        let (content_w, content_h) = self.content_size(self.zoom());
        let max_x = (content_w - f32::from(self.viewport_size.width)).max(0.);
        let max_y = (content_h - f32::from(self.viewport_size.height)).max(0.);
        let clamped = (self.scroll.0.clamp(0., max_x), self.scroll.1.clamp(0., max_y));
        if clamped != self.scroll {
            self.scroll = clamped;
            self.mark_dirty();
        }
    }

    // thumbs of the horizontal and vertical scrollbars (None = no scrollbar needed)
    fn scrollbar_layout(&self) -> (Option<Thumb>, Option<Thumb>) {
        let (content_w, content_h) = self.content_size(self.zoom());
        let view_w = f32::from(self.viewport_size.width);
        let view_h = f32::from(self.viewport_size.height);
        let need_x = content_w - view_w > 0.5;
        let need_y = content_h - view_h > 0.5;
        // shorten the tracks so that the two bars do not overlap in the corner
        let track_x = view_w - if need_y { SCROLLBAR_SIZE } else { 0. };
        let track_y = view_h - if need_x { SCROLLBAR_SIZE } else { 0. };
        (
            thumb(content_w, view_w, self.scroll.0, track_x),
            thumb(content_h, view_h, self.scroll.1, track_y),
        )
    }

    // move the scroll position so that the thumb follows the pointer
    // (`pos` is the pointer position along the axis, in viewport coordinates)
    fn set_scroll_from_thumb(&mut self, vertical: bool, pos: f32, grab: f32) {
        let (x, y) = self.scrollbar_layout();
        let Some(t) = (if vertical { y } else { x }) else { return };
        let travel = t.track - t.len;
        if travel <= 0. {
            return;
        }
        let start = (pos - grab).clamp(0., travel);
        let scroll = start / travel * t.max;
        if vertical {
            self.scroll.1 = scroll;
        } else {
            self.scroll.0 = scroll;
        }
        self.mark_dirty();
    }

    // mouse down on a scrollbar track: grab the thumb, or center it on the pointer
    fn start_scrollbar_drag(&mut self, vertical: bool, pos: f32) {
        let (x, y) = self.scrollbar_layout();
        let Some(t) = (if vertical { y } else { x }) else { return };
        let grab = if pos >= t.start && pos <= t.start + t.len {
            pos - t.start
        } else {
            t.len / 2.
        };
        self.drag = if vertical { Drag::ScrollbarY { grab } } else { Drag::ScrollbarX { grab } };
        self.set_scroll_from_thumb(vertical, pos, grab);
    }

    // top-left corner of the canvas in viewport coordinates (logical pixels);
    // the canvas is centered inside the content
    fn canvas_origin(&self) -> (f32, f32) {
        let zoom = self.zoom();
        let (content_w, content_h) = self.content_size(zoom);
        (
            (content_w - self.doc.pixmap.width as f32 * zoom) / 2. - self.scroll.0,
            (content_h - self.doc.pixmap.height as f32 * zoom) / 2. - self.scroll.1,
        )
    }

    // zoom around the viewport center (used by the slider)
    fn apply_zoom(&mut self, new_percent: f32) {
        let anchor = (
            f32::from(self.viewport_size.width) / 2.,
            f32::from(self.viewport_size.height) / 2.,
        );
        self.apply_zoom_at(new_percent, anchor);
    }

    // change the zoom while keeping the canvas point under `anchor` (viewport coordinates,
    // logical pixels) in place
    fn apply_zoom_at(&mut self, new_percent: f32, anchor: (f32, f32)) {
        let new_percent = new_percent.clamp(MIN_ZOOM_PERCENT, MAX_ZOOM_PERCENT);
        // the canvas point (in canvas pixels) currently under the anchor
        let old_zoom = self.zoom();
        let (ox, oy) = self.canvas_origin();
        let cx = (anchor.0 - ox) / old_zoom;
        let cy = (anchor.1 - oy) / old_zoom;

        self.zoom_percent = new_percent;
        let new_zoom = self.zoom();

        // choose the scroll so that the same canvas point stays under the anchor
        let (content_w, content_h) = self.content_size(new_zoom);
        self.scroll.0 = (content_w - self.doc.pixmap.width as f32 * new_zoom) / 2. - (anchor.0 - cx * new_zoom);
        self.scroll.1 = (content_h - self.doc.pixmap.height as f32 * new_zoom) / 2. - (anchor.1 - cy * new_zoom);
        self.clamp_scroll();
        self.mark_dirty();
    }

    // zoom so that the whole canvas fits into the viewport
    fn fit_to_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let avail_w = f32::from(self.viewport_size.width) - CANVAS_MARGIN * 2.;
        let avail_h = f32::from(self.viewport_size.height) - CANVAS_MARGIN * 2.;
        let zoom = (avail_w / self.doc.pixmap.width as f32).min(avail_h / self.doc.pixmap.height as f32);
        // round down to a whole percent so that the canvas is never cut off
        let percent = (zoom * 100.).floor().clamp(MIN_ZOOM_PERCENT, MAX_ZOOM_PERCENT);
        self.apply_zoom(percent);
        // keep the slider thumb in sync
        self.zoom_slider
            .update(cx, |state, cx| state.set_value(percent, window, cx));
        cx.notify();
    }

    fn fit_to_actual(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_zoom(100.);
        // keep the slider thumb in sync
        self.zoom_slider
            .update(cx, |state, cx| state.set_value(100., window, cx));
        cx.notify();
    }

    // convert a window position into canvas pixel coordinates (maybe outside the pixel map)
    fn to_canvas_px(&self, pos: Point<Pixels>) -> (i32, i32) {
        let origin = self.viewport_origin.get();
        let (ox, oy) = self.canvas_origin();
        let zoom = self.zoom();
        let x = (f32::from(pos.x - origin.x) - ox) / zoom;
        let y = (f32::from(pos.y - origin.y) - oy) / zoom;
        (x.floor() as i32, y.floor() as i32)
    }

    // -- view rendering ------------------------------------------------------

    // build the visible part of the pixel map, magnified with nearest-neighbor sampling
    fn rebuild_view_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let scale = window.scale_factor();
        let w = (f32::from(self.viewport_size.width) * scale).ceil().max(1.) as usize;
        let h = (f32::from(self.viewport_size.height) * scale).ceil().max(1.) as usize;

        // device pixels per canvas pixel, and the canvas origin in device pixels
        let zoom = self.zoom() * scale;
        let (ox, oy) = self.canvas_origin();
        let (ox, oy) = (ox * scale, oy * scale);

        // checkerboard parity per device column / row, anchored to the canvas origin
        let cell = CHECKER_CELL * scale;
        let col_par: Vec<i32> = (0..w).map(|i| ((i as f32 + 0.5 - ox) / cell).floor() as i32 & 1).collect();
        let row_par: Vec<i32> = (0..h).map(|i| ((i as f32 + 0.5 - oy) / cell).floor() as i32 & 1).collect();

        let bw = self.doc.pixmap.width as i32;
        let bh = self.doc.pixmap.height as i32;

        self.xs_cache.clear();
        self.xs_cache.reserve(w);
        for i in 0..w {
            let c = ((i as f32 + 0.5 - ox) / zoom).floor() as i32;
            self.xs_cache.push(if c < 0 || c >= bw { -1 } else { c });
        }

        self.ys_cache.clear();
        self.ys_cache.reserve(h);
        for i in 0..h {
            let c = ((i as f32 + 0.5 - oy) / zoom).floor() as i32;
            self.ys_cache.push(if c < 0 || c >= bh { -1 } else { c });
        }

        // lifted selection pixels are drawn on top of the pixel map
        let floating = self.doc.selection.as_ref()
            .and_then(|s| s.floating.as_ref().map(|f| (s.x, s.y, f)));

        let [bg_r, bg_g, bg_b] = to_rgb8(invert_lightness(cx.theme().muted_foreground));
        let grid = self.zoom() >= GRID_MIN_ZOOM;
        let mut buf = vec![0u8; w * h * 4];
        for y in 0..h {
            let cry = self.ys_cache[y];
            // first device row of a canvas pixel row
            let on_h_line = grid && cry >= 0 && (y == 0 || self.ys_cache[y - 1] != cry);
            let row = &mut buf[y * w * 4..(y + 1) * w * 4];
            for x in 0..w {
                let crx = self.xs_cache[x];
                if crx < 0 || cry < 0 {
                    row[x * 4] = bg_b;
                    row[x * 4 + 1] = bg_g;
                    row[x * 4 + 2] = bg_r;
                    row[x * 4 + 3] = 0xff;
                    continue;
                }

                let base = self.doc.pixmap.pixels[cry as usize * self.doc.pixmap.width + crx as usize];
                let p = match floating {
                    Some((sx, sy, f))
                    if crx >= sx && crx < sx + f.width as i32 && cry >= sy && cry < sy + f.height as i32 =>
                        {
                            // the lifted selection is shown composited over the pixel map
                            over(f.pixels[(cry - sy) as usize * f.width + (crx - sx) as usize], base)
                        }
                    _ => base,
                };
                let mut c = [p[0], p[1], p[2]];
                if p[3] != 255 {
                    // blend with the checkerboard behind transparent pixels
                    let chk = if (col_par[x] ^ row_par[y]) == 0 { CHECKER_LIGHT } else { CHECKER_DARK };
                    let a = p[3] as u32;
                    for k in 0..3 {
                        c[k] = ((c[k] as u32 * a + chk[k] as u32 * (255 - a) + 127) / 255) as u8;
                    }
                }

                // first device column of a canvas pixel column
                if grid && (on_h_line || x == 0 || self.xs_cache[x - 1] != crx) {
                    for v in &mut c {
                        *v = ((*v as u16 + 0x80) / 2) as u8;
                    }
                }
                // gpui expects BGRA
                row[x * 4] = c[2];
                row[x * 4 + 1] = c[1];
                row[x * 4 + 2] = c[0];
                row[x * 4 + 3] = 0xff;
            }
        }

        // selection outline: a dashed black/white rectangle, one device pixel wide
        if let Some(sel) = &self.doc.selection {
            let x0 = (ox + sel.x as f32 * zoom).round() as i32;
            let y0 = (oy + sel.y as f32 * zoom).round() as i32;
            let x1 = ((ox + (sel.x + sel.w) as f32 * zoom).round() as i32 - 1).max(x0);
            let y1 = ((oy + (sel.y + sel.h) as f32 * zoom).round() as i32 - 1).max(y0);
            let mut put = |x: i32, y: i32, i: i32| {
                if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
                    return;
                }
                let v = if (i / 4) % 2 == 0 { 0u8 } else { 255u8 };
                let o = (y as usize * w + x as usize) * 4;
                buf[o] = v;
                buf[o + 1] = v;
                buf[o + 2] = v;
                buf[o + 3] = 0xff;
            };
            for x in x0..=x1 {
                put(x, y0, x - x0);
                put(x, y1, x - x0);
            }
            for y in y0..=y1 {
                put(x0, y, y - y0);
                put(x1, y, y - y0);
            }
        }

        let buffer = image::RgbaImage::from_raw(w as u32, h as u32, buf)
            .expect("buffer size matches the image size");
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
        // release the previous frame's texture, or the atlas would keep growing
        if let Some(old) = self.view_image.replace(image) {
            let _ = window.drop_image(old);
        }
        self.view_pixels = (w, h);
        self.view_dirty = false;
    }

    // -- history -------------------------------------------------------------

    fn after_history_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_size_inputs(window, cx);
        self.clamp_scroll();
        self.mark_dirty();
        cx.notify();
    }

    fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let changed = self.doc.undo();
        self.mark_dirty();
        if changed {
            self.after_history_change(window, cx);
        } else {
            cx.notify(); // a floating selection may have been flattened
        }
    }

    fn redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let changed = self.doc.redo();
        self.mark_dirty();
        if changed {
            self.after_history_change(window, cx);
        } else {
            cx.notify();
        }
    }

    // replace the whole pixel map (resize / open); `record` makes it undoable
    fn replace_pixmap(&mut self, new_pixmap: Pixmap, record: bool) {
        self.drag = Drag::None;
        self.doc.replace_pixmap(new_pixmap, record);
    }

    // -- selection -----------------------------------------------------------

    // flatten the selection into the pixel map (a no-op if the pixels were never lifted)
    fn commit_selection(&mut self) {
        self.doc.commit_selection();
        self.mark_dirty();
    }

    // select the whole canvas
    fn select_all(&mut self, cx: &mut Context<Self>) {
        // ignore the shortcut while a drag (stroke, shape, selection, scrollbar) is in progress
        if !matches!(self.drag, Drag::None) {
            return;
        }
        self.doc.select_all();
        self.painter.tool = Tool::Select;
        self.mark_dirty();
        cx.notify();
    }

    fn delete_selection(&mut self, cx: &mut Context<Self>) {
        self.doc.delete_selection();
        self.mark_dirty();
        cx.notify();
    }

    // Esc: drop the selection, keeping the moved pixels in place
    fn deselect(&mut self, cx: &mut Context<Self>) {
        if matches!(self.drag, Drag::SelectNew { .. } | Drag::SelectMove { .. } | Drag::MosaicNew { .. }) {
            self.drag = Drag::None;
        }
        self.commit_selection();
        self.over_selection = false;
        self.mark_dirty();
        cx.notify();
    }

    // copy the selection to the system clipboard as an image
    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let Some(region) = self.doc.selection_pixmap() else { return };

        // encode as PNG and put it on the clipboard through gpui
        let png = match region.to_png() {
            Ok(png) => png,
            Err(e) => {
                show_error(&image_error_message(&e));
                return;
            }
        };

        // Windows: write CF_DIB directly so that every app can paste the image;
        // fall back to gpui when the native path fails
        #[cfg(windows)]
        {
            match win_clipboard::copy_image(region.width, region.height, &region.pixels, &png) {
                Ok(()) => return,
                Err(e) => eprintln!("native clipboard failed, falling back to gpui: {e}"),
            }
        }

        cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(ImageFormat::Png, png)));
    }

    // read an image from the clipboard; Ok(None) means there is no image to paste
    fn read_clipboard_image(cx: &mut Context<Self>) -> Result<Option<image::RgbaImage>, String> {
        // Windows: read the native pixel map formats first (CF_DIBV5 / CF_DIB / "PNG")
        #[cfg(windows)]
        {
            if let Some(img) = win_clipboard::read_image() {
                return Ok(Some(img));
            }
        }

        // other platforms, or Windows with no native image: use the entries gpui provides
        let Some(item) = cx.read_from_clipboard() else { return Ok(None) };
        let Some(clip_image) = item.entries().iter().find_map(|entry| match entry {
            ClipboardEntry::Image(image) => Some(image),
            _ => None,
        }) else {
            return Ok(None);
        };
        image::load_from_memory(&clip_image.bytes)
            .map(|img| Some(img.to_rgba8()))
            .map_err(|e| t!("error.clipboard_read", error = e).to_string())
    }

    // paste the clipboard image as a floating selection that can be moved before it is committed
    fn paste_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {

        let decoded = match Self::read_clipboard_image(cx) {
            Ok(Some(img)) => img,
            // the clipboard holds no image (for example text): nothing to paste
            Ok(None) => return,
            Err(message) => {
                show_error(&message);
                return;
            }
        };
        let (w, h) = (decoded.width() as usize, decoded.height() as usize);
        if !is_valid_canvas_size(w, h) {
            show_error(&t!("error.clipboard_size", max_size = MAX_CANVAS_SIZE));
            return;
        }
        let pasted = Pixmap::from_image(decoded);

        // flatten the previous selection first
        self.commit_selection();

        // grow the canvas when the image does not fit (undoable); the size was validated above
        if matches!(self.doc.grow_to(w, h), Ok(true)) {
            self.drag = Drag::None;
            self.sync_size_inputs(window, cx);
        }

        // the canvas may have just grown, so make sure the scroll position is valid first
        self.clamp_scroll();

        // part of the canvas that is actually visible, in whole canvas pixels;
        // the scrollbars are drawn over the viewport, so the area under them does not count
        let (bar_x, bar_y) = self.scrollbar_layout();
        let view_w = f32::from(self.viewport_size.width) - if bar_y.is_some() { SCROLLBAR_SIZE } else { 0. };
        let view_h = f32::from(self.viewport_size.height) - if bar_x.is_some() { SCROLLBAR_SIZE } else { 0. };
        let (ox, oy) = self.canvas_origin();
        let zoom = self.zoom();
        let (bw, bh) = (self.doc.pixmap.width as i32, self.doc.pixmap.height as i32);
        let vx0 = ((-ox / zoom).ceil() as i32).clamp(0, bw);
        let vy0 = ((-oy / zoom).ceil() as i32).clamp(0, bh);
        let vx1 = (((view_w - ox) / zoom).floor() as i32).clamp(vx0, bw);
        let vy1 = (((view_h - oy) / zoom).floor() as i32).clamp(vy0, bh);

        // Center the image in the visible range when it fits there, so it is fully visible.
        // Otherwise, align it to the start of the visible range, so that it covers the view.
        // The result is always clamped into the canvas, so the paste position never grows it.
        let place = |lo: i32, hi: i32, len: i32, max: i32| -> i32 {
            let pos = if len <= hi - lo { lo + (hi - lo - len) / 2 } else { lo };
            pos.clamp(0, max.max(0))
        };
        let x = place(vx0, vx1, w as i32, bw - w as i32);
        let y = place(vy0, vy1, h as i32, bh - h as i32);

        self.painter.tool = Tool::Select;
        self.doc.paste_floating(pasted, x, y);
        self.mark_dirty();
        cx.notify();
    }

    fn select_mouse_down(&mut self, p: (i32, i32)) {
        if let Some(grab) = self.doc.grab_selection(p) {
            self.drag = Drag::SelectMove { grab };
        } else {
            self.commit_selection();
            self.drag = Drag::SelectNew { start: p };
        }
    }

    // -- drawing -------------------------------------------------------------

    fn draw_text(&mut self, origin: (i32, i32), window: &mut Window, cx: &mut Context<Self>) {

        let app = cx.entity();
        let default_font_size = (self.painter.brush_size * 8).clamp(12, 120);
        let text_input = cx.new(|cx| InputState::new(window, cx).placeholder("Enter text..."));
        let font_size_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(default_font_size.to_string())
                .step(2.)
                .min(6.)
                .max(500.)
        });

        // font select: preselect the font used last time,
        // or the system default (sans-serif) font on the first use
        let families = self.text.families().to_vec();
        let preferred = self.text.last_font.clone().or_else(|| self.text.default_family());
        let selected = preferred
            .as_ref()
            .and_then(|name| families.iter().position(|f| f == name))
            .unwrap_or(0);
        let font_select = cx.new(|cx| {
            SelectState::new(families, Some(IndexPath::new(selected)), window, cx)
        });

        let initial_focus = Cell::new(true);

        window.open_dialog(cx, move |dialog, window, cx| {
            if initial_focus.replace(false) {
                text_input.focus_handle(cx).focus(window, cx);
            }

            // Shared by the OK button and the Enter key.
            // Returns true when the dialog should be closed.
            let submit: Rc<dyn Fn(&mut Window, &mut App) -> bool> = {
                let app = app.clone();
                let text_input = text_input.clone();
                let font_size_input = font_size_input.clone();
                let font_select = font_select.clone();
                Rc::new(move |_window: &mut Window, cx: &mut App| -> bool {
                    let text = text_input.read(cx).value().to_string();
                    // reject invalid sizes instead of silently falling back to a default
                    let Ok(font_size) = font_size_input.read(cx).value().parse::<f32>() else {
                        show_error(&t!("error.font_size"));
                        return false;
                    };
                    let font_size = font_size.clamp(6.0, 500.0);
                    let family: Option<String> = font_select.read(cx).selected_value().cloned();
                    if !text.is_empty() {
                        app.update(cx, |this, cx| {
                            this.text.last_font = family.clone();
                            this.render_text(&text, font_size, family.as_deref(), origin, cx);
                        });
                    }
                    true
                })
            };

            let row = |label: &'static str, input: &Entity<InputState>| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(div().w(px(56.)).text_sm().child(label))
                    .child(div().flex_1().child(NumberInput::new(input)))
            };

            let submit_for_button = submit.clone();
            let submit_for_enter = submit.clone();

            dialog
                .title("Draw text")
                .w(px(320.))
                // Enter in an input runs the same action as the OK button
                .on_ok(move |_, window, cx| submit_for_enter(window, cx))
                .on_cancel(|_, _, _| true)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.))
                                .child(div().w(px(56.)).text_sm().child("Text"))
                                .child(div().flex_1().child(Input::new(&text_input))),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.))
                                .child(div().w(px(56.)).text_sm().child("Font"))
                                .child(div().flex_1().child(Select::new(&font_select))),
                        )
                        .child(row("Size", &font_size_input))
                        .child(div().text_xs().child("Text will be drawn at the clicked position.")),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("text-ok")
                                .primary()
                                .label("OK")
                                .on_click(move |_, window, cx| {
                                    if submit_for_button(window, cx) {
                                        window.close_dialog(cx);
                                    }
                                }),
                        )
                        .child(
                            Button::new("text-cancel")
                                .label("Cancel")
                                .on_click(|_, window, cx| {
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn render_text(
        &mut self,
        text: &str,
        font_size: f32,
        family: Option<&str>,
        origin: (i32, i32),
        cx: &mut Context<Self>,
    ) {
        let drawn = self.text.render(
            &mut self.doc.pixmap,
            text,
            font_size,
            family,
            origin,
            self.painter.color,
            self.painter.antialias,
        );
        if drawn {
            self.doc.record_history();
            self.mark_dirty();
            cx.notify();
        }
    }

    // -- canvas size ---------------------------------------------------------

    fn sync_size_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (width, height) = (self.doc.pixmap.width, self.doc.pixmap.height);
        self.set_size_inputs(width, height, window, cx);
    }

    // show the given size in the width / height inputs
    fn set_size_inputs(&mut self, width: usize, height: usize, window: &mut Window, cx: &mut Context<Self>) {
        let width = width.to_string();
        let height = height.to_string();
        self.width_input.update(cx, |state, cx| state.set_value(width, window, cx));
        self.height_input.update(cx, |state, cx| state.set_value(height, window, cx));
    }

    fn apply_resize(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {

        let width = self.width_input.read(cx).value().parse::<usize>().ok();
        let height = self.height_input.read(cx).value().parse::<usize>().ok();
        let (Some(width), Some(height)) = (width, height) else {
            show_error(&t!("error.size_not_number"));
            return false;
        };
        // the trim origin is the top-left corner of the selection (see Document::trim);
        // the range is validated before the selection is touched, so that a rejected
        // input leaves the selection intact for the next try
        match self.doc.trim(width, height) {
            Ok(true) => {}
            Ok(false) => return false,
            Err(SizeError) => {
                show_error(&t!("error.size_range", max_size = MAX_CANVAS_SIZE));
                return false;
            }
        }
        self.drag = Drag::None;
        self.mark_dirty();
        self.after_history_change(window, cx);
        true
    }

    fn open_resize_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // show the selection size in the inputs, or the canvas size when nothing is selected
        let trim = self.doc.selection.is_some();
        match self.doc.selection.as_ref().map(|s| (s.w as usize, s.h as usize)) {
            Some((w, h)) => self.set_size_inputs(w, h, window, cx),
            None => self.sync_size_inputs(window, cx),
        }

        // the dialog builder may run again, so it only captures cheap handles
        let app = cx.entity();
        let width_input = self.width_input.clone();
        let height_input = self.height_input.clone();
        let initial_focus = Cell::new(true);

        window.open_dialog(cx, move |dialog, window, cx| {
            // focus the width field once, when the dialog opens
            if initial_focus.replace(false) {
                width_input.focus_handle(cx).focus(window, cx);
            }

            let row = |label: &'static str, input: &Entity<InputState>| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(div().w(px(56.)).text_sm().child(label))
                    .child(div().flex_1().child(NumberInput::new(input)))
            };

            let app = app.clone();
            // Shared by the OK button and the Enter key.
            // Returns true when the dialog should be closed (only if the size was valid).
            let app_for_enter = app.clone();
            let app_for_button = app.clone();

            dialog
                .title(if trim { t!("dialog.trim.title") } else { t!("dialog.resize.title") })
                .w(px(320.))
                // Enter in an input runs the same action as the OK button
                .on_ok(move |_, window, cx| {
                    app_for_enter.update(cx, |this, cx| this.apply_resize(window, cx))
                })
                .on_cancel(|_, _, _| true)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(row("Width", &width_input))
                        .child(row("Height", &height_input))
                        .child(div().text_xs().child(if trim {
                            t!("dialog.trim.msg", max_size = MAX_CANVAS_SIZE)
                        } else {
                            t!("dialog.resize.msg", max_size = MAX_CANVAS_SIZE)
                        })),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("resize-ok")
                                .primary()
                                .label("OK")
                                .on_click(move |_, window, cx| {
                                    // close only when the size was valid
                                    let applied = app_for_button.update(cx, |this, cx| this.apply_resize(window, cx));
                                    if applied {
                                        window.close_dialog(cx);
                                    }
                                }),
                        )
                        .child(
                            Button::new("resize-cancel")
                                .label("Cancel")
                                .on_click(|_, window, cx| {
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    // -- input handlers ------------------------------------------------------

    fn handle_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // also ignore the left button while panning
        if self.is_saving || matches!(self.drag, Drag::Pan { .. }) {
            return;
        }
        self.focus_canvas(window, cx);
        let p = self.to_canvas_px(event.position);
        let tool = self.painter.tool;
        // every tool except Select flattens an active selection first
        if tool != Tool::Select {
            self.commit_selection();
        }
        match tool {
            Tool::Pencil | Tool::Eraser => {
                self.drag = Drag::Draw;
                self.last_pos = p;
                self.painter.begin_stroke(&mut self.doc.pixmap, p);
            }
            Tool::Fill => {
                self.painter.fill(&mut self.doc.pixmap, p);
                self.doc.record_history();
            }
            Tool::Picker => {
                if let Some(c) = self.doc.pixmap.get(p.0, p.1) {
                    self.painter.color = c;
                    let hsla = rgba_to_hsla(c);
                    // keep the picker's displayed color in sync (does not emit Change)
                    self.color_picker
                        .update(cx, |state, cx| state.set_value(hsla, window, cx));
                    // like classic Paint, go back to drawing after picking
                    self.painter.tool = Tool::Pencil;
                }
            }
            Tool::Line | Tool::Rectangle | Tool::Oval | Tool::Arrow => {
                self.drag = Drag::Shape { start: p };
                self.last_pos = p;
                self.painter.begin_shape(&mut self.doc.pixmap, p);
            }
            Tool::Text => {
                if self.doc.pixmap.get(p.0, p.1).is_some() {
                    self.draw_text(p, window, cx);
                }
            }
            Tool::Mosaic => {
                self.drag = Drag::MosaicNew { start: p };
                // show the rectangle with the selection outline while dragging
                self.doc.select_rect(p, p);
            }
            Tool::Select => self.select_mouse_down(p),
        }
        self.mark_dirty();
        cx.notify();
    }

    fn handle_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_saving {
            return;
        }
        let p = self.to_canvas_px(event.position);

        // cursor position shown in the footer
        let inside = p.0 >= 0 && p.1 >= 0 && p.0 < self.doc.pixmap.width as i32 && p.1 < self.doc.pixmap.height as i32;
        let cursor = inside.then_some(p);
        let mut changed = false;
        if cursor != self.cursor_px {
            self.cursor_px = cursor;
            changed = true;
        }
        // the pointer shape depends on whether the pointer is over the selection
        // (only the Select tool can move it)
        let over = self.painter.tool == Tool::Select && self.doc.selection.as_ref().is_some_and(|s| s.contains(p));
        if over != self.over_selection {
            self.over_selection = over;
            changed = true;
        }
        match self.drag {
            Drag::None => {}
            Drag::Draw => {
                if p != self.last_pos {
                    self.painter.stroke_to(&mut self.doc.pixmap, self.last_pos, p);
                    self.last_pos = p;
                    self.mark_dirty();
                    changed = true;
                }
            }
            Drag::Shape { start } => {
                if p != self.last_pos {
                    self.last_pos = p;
                    self.painter.update_shape(&mut self.doc.pixmap, start, p);
                    self.mark_dirty();
                    changed = true;
                }
            }
            Drag::MosaicNew { start } => {
                self.doc.select_rect(start, p);
                self.mark_dirty();
                changed = true;
            }
            Drag::SelectNew { start } => {
                self.doc.select_rect(start, p);
                self.mark_dirty();
                changed = true;
            }
            Drag::SelectMove { grab } => {
                if let Some(sel) = self.doc.selection.as_mut() {
                    sel.x = p.0 - grab.0;
                    sel.y = p.1 - grab.1;
                    self.mark_dirty();
                    changed = true;
                }
            }
            Drag::ScrollbarX { grab } => {
                let origin = self.viewport_origin.get();
                self.set_scroll_from_thumb(false, f32::from(event.position.x - origin.x), grab);
                changed = true;
            }
            Drag::ScrollbarY { grab } => {
                let origin = self.viewport_origin.get();
                self.set_scroll_from_thumb(true, f32::from(event.position.y - origin.y), grab);
                changed = true;
            }
            Drag::Pan { last } => {
                let now = (f32::from(event.position.x), f32::from(event.position.y));
                self.scroll.0 -= now.0 - last.0;
                self.scroll.1 -= now.1 - last.1;
                self.drag = Drag::Pan { last: now };
                self.clamp_scroll();
                self.mark_dirty();
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    fn handle_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        // releasing the left button must not end a pan
        if self.is_saving || matches!(self.drag, Drag::Pan { .. }) {
            return;
        }
        match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::Draw | Drag::Shape { .. } => {
                self.painter.finish();
                self.doc.record_history();
            }
            Drag::MosaicNew { .. } => {
                // apply the mosaic to the dragged rectangle; the rectangle is not kept as a selection
                if let Some(sel) = self.doc.selection.take() {
                    if sel.w > 1 || sel.h > 1 {
                        self.painter.mosaic(&mut self.doc.pixmap, sel.x, sel.y, sel.w, sel.h);
                        self.doc.record_history();
                    }
                }
            }
            Drag::SelectNew { .. } => {
                // a plain click (no drag) just clears the selection
                if self.doc.selection.as_ref().is_some_and(|s| s.w == 1 && s.h == 1) {
                    self.doc.selection = None;
                }
            }
            _ => {}
        }

        // the selection may have just been created, moved or cleared
        let p = self.to_canvas_px(event.position);
        self.over_selection = self.painter.tool == Tool::Select
            && self.doc.selection.as_ref().is_some_and(|s| s.contains(p));

        self.mark_dirty();
        cx.notify();
    }

    // start panning with the right button (only when nothing else is being dragged)
    fn handle_right_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_saving || !matches!(self.drag, Drag::None) {
            return;
        }
        self.focus_canvas(window, cx);
        self.drag = Drag::Pan {
            last: (f32::from(event.position.x), f32::from(event.position.y)),
        };
        cx.notify(); // refresh the pointer shape
    }

    // stop panning when the right button is released
    fn handle_right_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.drag, Drag::Pan { .. }) {
            self.drag = Drag::None;
            cx.notify();
        }
    }

    // end an in-progress stroke or shape the same way a mouse release does,
    // so that its pixels are recorded in the history before any discard check
    fn finish_drag(&mut self) {
        match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::Draw | Drag::Shape { .. } => {
                self.painter.finish();
                self.doc.record_history();
            }
            // cancel an unfinished mosaic drag without applying it
            Drag::MosaicNew { .. } => {
                self.doc.selection = None;
                self.mark_dirty();
            }
            _ => {}
        }
    }

    // a file was dropped from the OS onto the viewport
    fn handle_drop(&mut self, paths: &ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        // open the first dropped file; the others are ignored
        if let Some(path) = paths.paths().first() {
            self.check_discard_and_run(DiscardAction::LoadImage(path.clone()), window, cx);
            self.focus_canvas(window, cx);
        }
    }

    fn handle_scroll_wheel(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_saving {
            return;
        }
        let delta = event.delta.pixel_delta(px(20.));

        // Ctrl + wheel: zoom around the mouse pointer instead of scrolling
        if event.modifiers.secondary() {
            let origin = self.viewport_origin.get();
            let anchor = (
                f32::from(event.position.x - origin.x),
                f32::from(event.position.y - origin.y),
            );
            // exponential steps feel the same at every zoom level (one wheel notch is about 10%),
            // and smooth touchpad deltas work too
            let percent = self.zoom_percent * (f32::from(delta.y) * 0.005).exp();
            self.apply_zoom_at(percent, anchor);

            // keep the slider thumb in sync
            let percent = self.zoom_percent;
            self.zoom_slider
                .update(cx, |state, cx| state.set_value(percent, window, cx));
            cx.notify();
            return;
        }

        // Shift + wheel: scroll horizontally (mice without a horizontal wheel)
        let (dx, dy) = if event.modifiers.shift && delta.x == px(0.) {
            (f32::from(delta.y), 0.)
        } else {
            (f32::from(delta.x), f32::from(delta.y))
        };
        self.scroll.0 -= dx;
        self.scroll.1 -= dy;
        self.clamp_scroll();
        self.mark_dirty();
        cx.notify();
    }

    fn handle_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_saving {
            return;
        }
        // the controls keep their own keyboard handling
        if self.size_select.focus_handle(cx).is_focused(window)
        || self.color_picker.focus_handle(cx).is_focused(window)
        || self.width_input.focus_handle(cx).is_focused(window)
        || self.height_input.focus_handle(cx).is_focused(window) {
            return;
        }

        let key = &event.keystroke;
        let with_control = if cfg!(target_os = "macos") { key.modifiers.platform } else { key.modifiers.control };
        if with_control {
            match key.key.as_str() {
                "z" if key.modifiers.shift => self.redo(window, cx),
                "z" => self.undo(window, cx),
                "y" => self.redo(window, cx),
                "a" => self.select_all(cx),
                "c" => self.copy_selection(cx),
                "v" => self.paste_image(window, cx),
                "s" => self.save_image(key.modifiers.shift, None, window, cx),
                "n" => self.check_discard_and_run(DiscardAction::NewDocument, window, cx),
                "o" => self.open_image_dialog(window, cx),
                _ => {}
            }
        } else if self.focus_handle.is_focused(window) {
            // plain keys only when the canvas itself has the focus, so that Esc / Delete
            // inside popovers and dropdowns are left alone
            match key.key.as_str() {
                "delete" => self.delete_selection(cx),
                "escape" => self.deselect(cx),
                _ => {}
            }
        }
    }

    // -- MCP -----------------------------------------------------------------

    fn handle_mcp(&mut self, req: McpRequest, window: &mut Window, cx: &mut Context<Self>) {
        let McpRequest { op, reply } = req;
        // the caller gave up waiting (timeout): do not run the operation late
        if reply.is_canceled() {
            return;
        }
        let result = if self.is_saving {
            Err("the application is busy: a save dialog is open".to_string())
        } else {
            self.run_mcp_op(op, window, cx)
        };
        let _ = reply.send(result);
    }

    fn run_mcp_op(&mut self, op: McpOp, window: &mut Window, cx: &mut Context<Self>) -> Result<McpReply, String> {
        let is_edit = op.is_edit();
        if is_edit {
            // end whatever the user is doing, so that the history stays consistent
            self.finish_drag();
            self.commit_selection();
        }
        let result = self.exec_mcp_op(op, window, cx);
        if is_edit {
            self.mark_dirty();
            cx.notify();
        }
        result
    }

    /// Run `f` with the painter settings of `style` applied for this call only.
    fn with_style<R>(&mut self, style: &Style, f: impl FnOnce(&mut Self) -> R) -> R {
        let p = &mut self.painter;
        let saved = (p.color, p.brush_size, p.shape_filled, p.antialias);
        if let Some(v) = style.color {
            p.color = v;
        }
        if let Some(v) = style.size {
            p.brush_size = v;
        }
        if let Some(v) = style.filled {
            p.shape_filled = v;
        }
        if let Some(v) = style.antialias {
            p.antialias = v;
        }
        let result = f(self);
        let p = &mut self.painter;
        (p.color, p.brush_size, p.shape_filled, p.antialias) = saved;
        result
    }

    fn exec_mcp_op(&mut self, op: McpOp, window: &mut Window, cx: &mut Context<Self>) -> Result<McpReply, String> {
        match op {
            McpOp::GetCanvasInfo => Ok(McpReply::Info(CanvasInfo {
                width: self.doc.pixmap.width,
                height: self.doc.pixmap.height,
                color: mcp::format_color(self.painter.color),
                brush_size: self.painter.brush_size,
                antialias: self.painter.antialias,
                modified: self.doc.is_modified(),
                file: self.doc.file_path.as_ref().map(|p| p.display().to_string()),
            })),
            McpOp::GetImage => {
                // show a lifted selection as part of the image, without touching the user's state
                let mut pixmap = self.doc.pixmap.clone();
                if let Some(sel) = &self.doc.selection {
                    if let Some(floating) = &sel.floating {
                        pixmap.paste(floating, sel.x, sel.y);
                    }
                }
                pixmap.to_png().map(McpReply::Png).map_err(|e| e.to_string())
            }
            McpOp::ListFonts => Ok(McpReply::Fonts(self.text.families().to_vec())),
            McpOp::SetColor(rgba) => {
                self.painter.color = rgba;
                // keep the picker's displayed color in sync (does not emit Change)
                let hsla = rgba_to_hsla(rgba);
                self.color_picker.update(cx, |state, cx| state.set_value(hsla, window, cx));
                cx.notify();
                Ok(McpReply::Done)
            }
            McpOp::Shape { tool, from, to, style } => {
                self.with_style(&style, |this| this.painter.draw_shape_once(&mut this.doc, tool, from, to));
                Ok(McpReply::Done)
            }
            McpOp::Fill { at, style } => {
                self.with_style(&style, |this| this.painter.fill(&mut this.doc.pixmap, at));
                self.doc.record_history();
                Ok(McpReply::Done)
            }
            McpOp::Mosaic { x, y, w, h, block } => {
                self.doc.pixmap.mosaic(x, y, w, h, block);
                self.doc.record_history();
                Ok(McpReply::Done)
            }
            McpOp::Text { text, at, font_size, font, style } => {
                if let Some(name) = &font {
                    if !self.text.families().iter().any(|f| f == name) {
                        return Err(format!("font {name:?} is not installed (see list_fonts)"));
                    }
                }
                let drawn = self.with_style(&style, |this| {
                    this.text.render(
                        &mut this.doc.pixmap,
                        &text,
                        font_size,
                        font.as_deref(),
                        at,
                        this.painter.color,
                        this.painter.antialias,
                    )
                });
                if !drawn {
                    return Err("nothing was drawn (is the position inside the canvas?)".to_string());
                }
                if font.is_some() {
                    self.text.last_font = font;
                }
                self.doc.record_history();
                Ok(McpReply::Done)
            }
            McpOp::Undo => {
                if self.doc.undo() {
                    self.after_history_change(window, cx);
                    Ok(McpReply::Done)
                } else {
                    Err("nothing to undo".to_string())
                }
            }
            McpOp::Redo => {
                if self.doc.redo() {
                    self.after_history_change(window, cx);
                    Ok(McpReply::Done)
                } else {
                    Err("nothing to redo".to_string())
                }
            }
        }
    }
    
    // -- files ---------------------------------------------------------------

    fn handle_window_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.is_saving {
            return false;
        }
        self.finish_drag();
        self.commit_selection();
        if !self.doc.is_modified() {
            return true;
        }
        let answer = confirm_unsaved();
        match answer {
            rfd::MessageDialogResult::Yes => {
                let existing = self.doc.file_path.clone().filter(|p| save_format(p).is_some());
                if let Some(path) = existing {
                    let pixmap = self.doc.pixmap.clone();
                    let depth = self.doc.undo_depth();
                    self.write_image_data(&pixmap, depth, path, cx)
                } else {
                    self.save_image_on_close(cx);
                    false
                }
            }
            rfd::MessageDialogResult::No => true,
            _ => false,
        }
    }

    fn save_image_on_close(&mut self, cx: &mut Context<Self>) {
        let dialog = self.prepare_save_dialog();
        let current_pixmap = self.doc.pixmap.clone();
        let current_depth = self.doc.undo_depth();
        self.is_saving = true;

        cx.spawn(async move |this, cx| {
            let handle = dialog.save_file().await;
            this.update(cx, |this, cx| {
                this.is_saving = false;
                let Some(handle) = handle else {
                    return; // user canceled: leave the window open
                };
                let mut path = handle.path().to_path_buf();
                if path.extension().is_none() {
                    path = path.with_extension("png");
                }
                if this.write_image_data(&current_pixmap, current_depth, path, cx) {
                    cx.quit();
                }
            })
            .ok();
        })
        .detach();
    }

    fn check_discard_and_run(
        &mut self,
        action: DiscardAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_saving {
            return;
        }
        self.finish_drag();
        self.commit_selection();
        if !self.doc.is_modified() {
            self.perform_discard_action(action, window, cx);
            return;
        }
        let answer = confirm_unsaved();
        match answer {
            rfd::MessageDialogResult::Yes => {
                self.save_image(false, Some(action), window, cx);
            }
            rfd::MessageDialogResult::No => {
                self.perform_discard_action(action, window, cx);
            }
            _ => {}
        }
    }

    fn perform_discard_action(
        &mut self,
        action: DiscardAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            DiscardAction::NewDocument => {
                let pixmap = Pixmap::new(DEFAULT_CANVAS_SIZE.0, DEFAULT_CANVAS_SIZE.1, BACKGROUND_WHITE);
                self.replace_pixmap(pixmap, false);
                self.doc.file_path = None;
                self.scroll = (0., 0.);
                self.after_history_change(window, cx);
            }
            DiscardAction::LoadImage(path) => {
                let pixmap = match Pixmap::open(&path) {
                    Ok(pixmap) => pixmap,
                    Err(e) => {
                        show_error(&image_error_message(&e));
                        return;
                    }
                };
                self.replace_pixmap(pixmap, false);
                self.doc.file_path = Some(path);
                self.scroll = (0., 0.);
                self.after_history_change(window, cx);
            }
        }
    }

    fn prepare_save_dialog(&self) -> rfd::AsyncFileDialog {
        let mut dialog = rfd::AsyncFileDialog::new()
            .add_filter("PNG image", &["png"])
            .add_filter("JPEG image", &["jpg", "jpeg"])
            .add_filter("BMP image", &["bmp"])
            .add_filter("GIF image", &["gif"])
            .add_filter("TIFF image", &["tif", "tiff"])
            .add_filter("WebP image", &["webp"]);

        match &self.doc.file_path {
            Some(p) => {
                let ext = if save_format(p).is_some() {
                    p.extension().map(|e| e.to_string_lossy().into_owned())
                } else {
                    None
                }
                .unwrap_or_else(|| "png".to_string());
                let name = p
                    .file_stem()
                    .map(|s| format!("{}.{ext}", s.to_string_lossy()))
                    .unwrap_or_else(|| "drawing.png".to_string());
                if let Some(dir) = p.parent() {
                    dialog = dialog.set_directory(dir);
                }
                dialog.set_file_name(name)
            }
            None => dialog.set_file_name("drawing.png"),
        }
    }

    fn save_image(
        &mut self,
        save_as: bool,
        continuation: Option<DiscardAction>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_saving {
            return;
        }
        self.finish_drag();
        self.commit_selection();

        let existing = self.doc.file_path.clone().filter(|p| !save_as && save_format(p).is_some());
        if let Some(path) = existing {
            let pixmap = self.doc.pixmap.clone();
            let depth = self.doc.undo_depth();
            if self.write_image_data(&pixmap, depth, path, cx) {
                if let Some(action) = continuation {
                    self.perform_discard_action(action, window, cx);
                }
            }
            return;
        }

        let dialog = self.prepare_save_dialog();
        let current_pixmap = self.doc.pixmap.clone();
        let current_depth = self.doc.undo_depth();
        self.is_saving = true;

        cx.spawn(async move |this, cx| {
            let handle = dialog.save_file().await;
            this.update_in(cx, |this, window, cx| {
                this.is_saving = false;
                let Some(handle) = handle else {
                    return; // user canceled
                };
                let mut path = handle.path().to_path_buf();
                if path.extension().is_none() {
                    path = path.with_extension("png");
                }
                if this.write_image_data(&current_pixmap, current_depth, path, cx) {
                    if let Some(action) = continuation {
                        this.perform_discard_action(action, window, cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn write_image_data(
        &mut self,
        pixmap: &Pixmap,
        saved_depth: usize,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Err(e) = pixmap.save(&path) {
            show_error(&image_error_message(&e));
            return false;
        }
        self.doc.file_path = Some(path);
        self.doc.mark_saved(saved_depth);
        self.mark_dirty();
        cx.notify();
        true
    }

    fn open_image_dialog(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.is_saving {
            return;
        }
        let dialog = rfd::AsyncFileDialog::new()
            .add_filter("Image", &["png", "jpg", "jpeg", "bmp", "gif", "tif", "tiff", "webp"]);
        cx.spawn(async move |this, cx| {
            let Some(handle) = dialog.pick_file().await else {
                return; // canceled
            };
            let path = handle.path().to_path_buf();
            this.update_in(cx, |this, window, cx| {
                this.check_discard_and_run(DiscardAction::LoadImage(path), window, cx);
            })
            .ok();
        })
        .detach();
    }

    // quit after the same unsaved-changes check that closing the window uses;
    // when a save dialog is needed, the quit happens after the file is written
    fn request_quit(&mut self, cx: &mut Context<Self>) {
        if self.handle_window_close(cx) {
            cx.quit();
        }
    }

    // -- UI ------------------------------------------------------------------

    fn tool_button(&self, label: &'static str, icon: impl Into<Icon>, tool: Tool, cx: &mut Context<Self>) -> impl IntoElement {
        div().child(
            Toggle::new(label)
                .tooltip(label)
                .icon(icon)
                .small()
                .checked(self.painter.tool == tool)
                .on_click(cx.listener(move |this, _checked, _, cx| {
                    // leaving the selection tool flattens the selection
                    if tool != Tool::Select {
                        this.commit_selection();
                    }
                    this.painter.tool = tool;
                    this.mark_dirty();
                    cx.notify();
                })),
        )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {

        let new_button = Button::new("btn-new")
            .ghost().small().icon(IconName::File)
            .tooltip(if cfg!(target_os = "macos") { "New (Cmd+N)" } else { "New (Ctrl+N)" })
            .on_click(cx.listener(|this, _, window, cx| {
                this.check_discard_and_run(DiscardAction::NewDocument, window, cx);
            }));
        let open_button = Button::new("btn-open")
            .ghost().small().icon(IconName::FolderOpen)
            .tooltip(if cfg!(target_os = "macos") { "Open (Cmd+O)" } else { "Open (Ctrl+O)" })
            .on_click(cx.listener(|this, _, window, cx| this.open_image_dialog(window, cx)));
        let save_button = Button::new("btn-save")
            .ghost().small().icon(IconName::Save)
            .tooltip(if cfg!(target_os = "macos") { "Save (Cmd+S)" } else { "Save (Ctrl+S)" })
            .on_click(cx.listener(|this, _, window, cx| {
                this.save_image(false, None, window, cx);
            }));
        let save_as_button = Button::new("btn-save-as")
            .ghost().small().icon(IconName::SavePen)
            .tooltip(if cfg!(target_os = "macos") { "Save As (Cmd+Shift+S)" } else { "Save As (Ctrl+Shift+S)" })
            .on_click(cx.listener(|this, _, window, cx| {
                this.save_image(true, None, window, cx);
            }));
        let undo_button = Button::new("btn-undo")
            .ghost().small().icon(IconName::Undo2)
            .tooltip(if cfg!(target_os = "macos") { "Undo (Cmd+Z)" } else { "Undo (Ctrl+Z)" })
            .on_click(cx.listener(|this, _, window, cx| this.undo(window, cx)));
        let redo_button = Button::new("btn-redo")
            .ghost().small().icon(IconName::Redo2)
            .tooltip(if cfg!(target_os = "macos") { "Redo (Cmd+Y)" } else { "Redo (Ctrl+Y)" })
            .on_click(cx.listener(|this, _, window, cx| this.redo(window, cx)));

        let files = div()
            .w_full()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(2.))
            .child(new_button)
            .child(open_button)
            .child(save_button)
            .child(save_as_button)
            .child(undo_button)
            .child(redo_button);

        // tool buttons: 2 columns
        let tools = div()
            .w_full()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(2.))
            .child(self.tool_button("Pencil", IconName::Pencil, Tool::Pencil, cx))
            .child(self.tool_button("Eraser", IconName::Eraser, Tool::Eraser, cx))
            .child(self.tool_button("Fill", IconName::PaintBucket, Tool::Fill, cx))
            .child(self.tool_button("Pick", IconName::Pipette, Tool::Picker, cx))
            .child(self.tool_button("Line", IconName::Slash, Tool::Line, cx))
            .child(self.tool_button("Rect", IconName::Square, Tool::Rectangle, cx))
            .child(self.tool_button("Oval", IconName::Ellipse, Tool::Oval, cx))
            .child(self.tool_button("Text", IconName::Type, Tool::Text, cx))
            .child(self.tool_button("Arrow", IconName::MoveDownRight, Tool::Arrow, cx))
            .child(self.tool_button("Mosaic", IconName::LensConvex, Tool::Mosaic, cx))
            .child(self.tool_button("Select", IconName::SquareDashedMousePointer, Tool::Select, cx))
            .child(Button::new("btn-resize")
                .small().icon(IconName::ImageUpscale).tooltip("Resize…").ghost()
                .on_click(cx.listener(|this, _, window, cx| this.open_resize_dialog(window, cx)))
            );

        // rectangle / ellipse style
        let filled_toggle = Toggle::new("filled").tooltip("Filled").small()
            .icon(IconName::Droplets).checked(self.painter.shape_filled)
            .on_click(cx.listener(|this, checked, _, cx| {
                this.painter.shape_filled = *checked; cx.notify();
            }));

        // anti-aliasing on/off
        let aa_toggle = Toggle::new("antialias")
            .tooltip("Anti-alias")
            .small().icon(IconName::ALargeSmall)
            .checked(self.painter.antialias)
            .on_click(cx.listener(|this, checked, _, cx| {
                this.painter.antialias = *checked;
                cx.notify();
            }));

        let modes = div()
            .w_full()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(2.))
            .child(filled_toggle)
            .child(aa_toggle);

        // brush size select
        let brush_size = div().w_full()
            .child(Select::new(&self.size_select).xsmall());

        // current color indicator
        let current = ColorPicker::new(&self.color_picker).anchor(Anchor::TopRight);

        // palette grid: 2 columns, wrapped
        let mut grid = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(4.))
            .w(px(SWATCH_SIZE * 2. + 4.));

        for hex in PALETTE {
            let rgba = hex_to_rgba(hex);
            let hsla = rgba_to_hsla(rgba);
            let selected = self.painter.color[0..3] == rgba[0..3];
            grid = grid.child(
                div()
                    .w(px(SWATCH_SIZE))
                    .h(px(SWATCH_SIZE))
                    .bg(hsla)
                    .border_2()
                    .border_color(if selected { cx.theme().accent_foreground } else { cx.theme().border })
                    .cursor(CursorStyle::PointingHand)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            // change the drawing color for subsequent operations
                            this.painter.color = rgba;
                            // keep the picker's displayed color in sync (does not emit Change)
                            this.color_picker
                                .update(cx, |state, cx| state.set_value(hsla, window, cx));
                            cx.notify();
                        }),
                    ),
            );
        }

        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(8.))
            .p(px(10.))
            .bg(cx.theme().secondary)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(files)
            .child(div().w_full().h(px(1.)).bg(cx.theme().ring))
            .child(tools)
            .child(div().w_full().h(px(1.)).bg(cx.theme().ring))
            .child(modes)
            .child(brush_size)
            .child(current)
            .child(grid)
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {

        // file name for the footer (the full path goes into the tooltip)
        let full_path = self
            .doc.file_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "Untitled (not saved yet)".to_string());
        let base_name = self
            .doc.file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_string());
        let file_text = if self.doc.is_modified() { format!("*{base_name}") } else { base_name };

        let size_text = format!("{} × {} px", self.doc.pixmap.width, self.doc.pixmap.height);
        let cursor_text = self
            .cursor_px
            .map(|(x, y)| format!("{}, {}", x, y))
            .unwrap_or_default();

        // size of the selection (empty when nothing is selected)
        let selection_text = self.doc.selection.as_ref()
            .map(|s| format!("{} × {} px", s.w, s.h))
            .unwrap_or_default();

        let fit_button = Button::new("btn-fit")
            .xsmall()
            .icon(IconName::ScanSquare)
            .tooltip("Fit to window")
            .on_click(cx.listener(|this, _, window, cx| this.fit_to_window(window, cx)));

        let fit_actual_button = Button::new("btn-fit-actual")
            .xsmall()
            .icon(IconName::Scan)
            .tooltip("100%")
            .on_click(cx.listener(|this, _, window, cx| this.fit_to_actual(window, cx)));

        div()
            .w_full()
            .h(px(FOOTER_HEIGHT))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .bg(cx.theme().secondary)
            .border_t_1()
            .border_color(cx.theme().sidebar_border)
            // image size and cursor position, selection size
            .child(div().w(px(110.)).text_xs().child(size_text))
            .child(div().w(px(80.)).text_xs().child(cursor_text))
            .child(div().w(px(110.)).text_xs().child(selection_text))
            .child(
                div()
                    .id("file-name")
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .child(file_text)
                    .tooltip(move |window, cx| Tooltip::new(full_path.clone()).build(window, cx)),
            )
            // zoom
            .child(div()
                .w(px(140.))
                .flex_none() // do not let the footer's flex layout resize it
                .flex()
                .flex_row()
                .child(Slider::new(&self.zoom_slider)),
            )
            .child(fit_button)
            .child(fit_actual_button)
            .child(
                div()
                    .w(px(44.))
                    .text_xs()
                    .child(format!("{}%", self.zoom_percent.round() as i32)),
            )
    }
}

impl Render for Application {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {

        // the viewport is whatever remains of the window after the fixed sidebar and footer
        let window_size = window.viewport_size();
        self.viewport_size = size(
            (window_size.width - px(SIDEBAR_WIDTH)).max(px(1.)),
            (window_size.height - px(FOOTER_HEIGHT)).max(px(1.)),
        );
        self.clamp_scroll();

        // rebuild the magnified view only when something changed
        let scale = window.scale_factor();
        let wanted = (
            (f32::from(self.viewport_size.width) * scale).ceil().max(1.) as usize,
            (f32::from(self.viewport_size.height) * scale).ceil().max(1.) as usize,
        );
        if self.view_dirty || self.view_pixels != wanted || self.view_image.is_none() {
            self.rebuild_view_image(window, cx);
        }

        let image = self.view_image.clone();
        let viewport_origin = self.viewport_origin.clone();
        let sidebar = self.render_sidebar(cx);
        let footer = self.render_footer(cx);

        // pointer shape: closed hand while moving the selection, open hand over it
        let pointer = match self.drag {
            Drag::SelectMove { .. } | Drag::Pan { .. } => CursorStyle::ClosedHand,
            Drag::None if self.over_selection && self.doc.selection.is_some() => CursorStyle::OpenHand,
            _ => if self.painter.tool == Tool::Text { CursorStyle::IBeam } else { CursorStyle::Crosshair },
        };

        let (bar_x, bar_y) = self.scrollbar_layout();

        // viewport: shows the pre-rendered image; the handlers live here so a stroke
        // keeps following the mouse even when it leaves the canvas
        let viewport = div()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .cursor(pointer)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::handle_mouse_up))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::handle_right_mouse_down))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::handle_right_mouse_up))
            .on_mouse_up_out(MouseButton::Right, cx.listener(Self::handle_right_mouse_up))
            .on_scroll_wheel(cx.listener(Self::handle_scroll_wheel))
            .on_drop(cx.listener(Self::handle_drop))
            // the image is generated at device resolution, so it is shown 1:1
            .children(image.map(|image| {
                img(ImageSource::Render(image))
                    .size_full()
                    .object_fit(ObjectFit::Fill)
            }))
            // an invisible overlay that only records the viewport origin
            .child(
                canvas(
                    move |bounds, _, _| viewport_origin.set(bounds.origin),
                    |_, _, _, _| {},
                )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
            )
            // horizontal scrollbar
            .children(bar_x.map(|t| {
                div()
                    .absolute()
                    .left_0()
                    .bottom_0()
                    .w(px(t.track))
                    .h(px(SCROLLBAR_SIZE))
                    .bg(cx.theme().scrollbar)
                    .cursor(CursorStyle::Arrow)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, e: &MouseDownEvent, _, cx| {
                            // do not let the viewport start drawing
                            cx.stop_propagation();
                            let origin = this.viewport_origin.get();
                            this.start_scrollbar_drag(false, f32::from(e.position.x - origin.x));
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(2.))
                            .bottom(px(2.))
                            .left(px(t.start))
                            .w(px(t.len))
                            .rounded_full()
                            .bg(cx.theme().scrollbar_thumb),
                    )
            }))
            // vertical scrollbar
            .children(bar_y.map(|t| {
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .h(px(t.track))
                    .w(px(SCROLLBAR_SIZE))
                    .bg(cx.theme().scrollbar)
                    .cursor(CursorStyle::Arrow)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, e: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            let origin = this.viewport_origin.get();
                            this.start_scrollbar_drag(true, f32::from(e.position.y - origin.y));
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px(2.))
                            .right(px(2.))
                            .top(px(t.start))
                            .h(px(t.len))
                            .rounded_full()
                            .bg(cx.theme().scrollbar_thumb),
                    )
            }));

        div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(sidebar)
                    .child(viewport),
            )
            .child(footer)
    }
}

/// convert an RGBA pixel into gpui HSLA
fn rgba_to_hsla(rgba: [u8; 4]) -> Hsla {
    let r = rgba[0] as f32 / 255.;
    let g = rgba[1] as f32 / 255.;
    let b = rgba[2] as f32 / 255.;
    let a = rgba[3] as f32 / 255.;
    Rgba { r, g, b, a }.into()
}

/// unpack `0xRRGGBB` into `[r, g, b, 255]`
fn hex_to_rgba(hex: u32) -> [u8; 4] {
    [
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
        0xff,
    ]
}

/// convert a gpui color into an opaque RGBA pixel
fn to_rgba8(color: Hsla) -> [u8; 4] {
    let c: Rgba = color.into();
    let q = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    [q(c.r), q(c.g), q(c.b), q(c.a)]
}
/// convert a gpui color into an opaque RGB pixel
fn to_rgb8(color: Hsla) -> [u8; 3] {
    let c: Rgba = color.into();
    let q = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    [q(c.r), q(c.g), q(c.b)]
}
fn invert_lightness(color: Hsla) -> Hsla {
    Hsla {
        l: 1.0 - color.l,
        ..color
    }
}

/// Convert a `file://` URL delivered by the OS into a path (percent-decoded).
/// Anything that is not a file URL is ignored.
fn url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    // skip an optional host such as "localhost"; the path starts at the next '/'
    let path = &rest[rest.find('/')?..];

    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(b) = hex {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok().map(PathBuf::from)
}

/// ask whether unsaved changes should be saved first
fn confirm_unsaved() -> rfd::MessageDialogResult {
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title(env!("CARGO_PKG_NAME"))
        .set_description(t!("confirm.unsaved"))
        .set_buttons(rfd::MessageButtons::YesNoCancel)
        .show()
}

/// Localized message of an image error.
fn image_error_message(e: &ImageError) -> String {
    match e {
        ImageError::UnsupportedExtension => t!("error.unsupported_ext").to_string(),
        ImageError::Build => t!("error.build_image").to_string(),
        ImageError::Encode(msg) => t!("error.encode_image", error = msg).to_string(),
        ImageError::Save(msg) => t!("error.save_image", error = msg).to_string(),
        ImageError::Open(msg) => t!("error.open_image", error = msg).to_string(),
    }
}

fn show_error(message: &str) {
    // the release build has no console, so report errors with a dialog
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title(t!("dialog.error.title"))
        .set_description(message)
        .show();
}

/// override the accent color in dark mode, where the default is too close to the background
fn update_theme_color(cx: &mut App) {
    if Theme::global(cx).mode.is_dark() {
        Theme::update(cx, |theme| {
            theme.accent = theme.blue.darken(0.5);
        });
    }
}

#[derive(Default)]
struct Args {
    // the first argument that is not an option: the image file to open
    initial_file: Option<PathBuf>,
    mcp_port: Option<u16>,
    mcp_token: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut it = std::env::args_os().skip(1);
    while let Some(arg) = it.next() {
        match arg.to_str() {
            Some("--mcp-port") => {
                let value = it.next().and_then(|v| v.into_string().ok()).ok_or("--mcp-port needs a value")?;
                args.mcp_port = Some(value.parse().map_err(|_| format!("invalid port: {value}"))?);
            }
            Some("--mcp-token") => {
                let value = it.next().and_then(|v| v.into_string().ok()).ok_or("--mcp-token needs a value")?;
                args.mcp_token = Some(value);
            }
            _ => {
                if args.initial_file.is_none() {
                    args.initial_file = Some(PathBuf::from(arg));
                }
            }
        }
    }
    Ok(args)
}

/// Start the MCP server; errors are reported with a dialog (the release build has no console).
fn start_mcp(port: u16, token: Option<String>) -> Option<UnboundedReceiver<McpRequest>> {
    // the token may come from the environment, which keeps it out of the process list
    let token = token.or_else(|| std::env::var("MIN_PAINT_MCP_TOKEN").ok()).unwrap_or_default();
    if token.len() < 16 {
        show_error("The MCP server needs an access token of at least 16 characters \
                    (--mcp-token or the MIN_PAINT_MCP_TOKEN environment variable).");
        return None;
    }
    match mcp::start(port, token) {
        Ok(rx) => Some(rx),
        Err(e) => {
            show_error(&format!("Failed to start the MCP server on 127.0.0.1:{port}: {e}"));
            None
        }
    }
}

fn main() {

    // i18n
    if let Some(locale) = sys_locale::get_locale() {
        rust_i18n::set_locale(&locale);
    }

    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            show_error(&message);
            return;
        }
    };
    let Args { initial_file, mcp_port, mcp_token } = args;

    // MCP requests are forwarded to the UI through a channel (opt-in)
    let mcp_rx = mcp_port.and_then(|port| start_mcp(port, mcp_token));
    let mcp_rx = Rc::new(RefCell::new(mcp_rx));

    // open requests from the OS (Finder) are forwarded to the UI through a channel;
    // events that arrive before the window exists are buffered
    let (open_tx, open_rx) = futures::channel::mpsc::unbounded::<PathBuf>();
    let open_rx = Rc::new(RefCell::new(Some(open_rx)));

    let app = application().with_assets(AllAssets);
    app.on_open_urls(move |urls| {
        // must be registered before `run`, so that no request is missed at launch
        for url in urls {
            if let Some(path) = url_to_path(&url) {
                let _ = open_tx.unbounded_send(path);
            }
        }
    });

    app.run(move |cx| {
        init(cx);

        #[cfg(target_os = "macos")]
        set_app_menu(cx);

        let window_options = window_options(cx);
        open_window(window_options, cx, move |window, cx| {
            let open_rx = open_rx.borrow_mut().take();
            let mcp_rx = mcp_rx.borrow_mut().take();
            cx.new(|cx| Application::new(initial_file.clone(), open_rx, mcp_rx, window, cx))
        })
        .expect("failed to open window");

        cx.on_window_closed(|cx, _window_id| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        }).detach();

        cx.activate(true);
    });
}

actions!(min_paint, [Quit]);

/// Application menu shown in the macOS menu bar (Quit only, for now).
#[cfg(target_os = "macos")]
fn set_app_menu(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
    cx.set_menus(vec![Menu {
        name: env!("CARGO_PKG_NAME").into(),
        items: vec![MenuItem::action("Quit min-paint", Quit)],
        disabled: false,
    }]);
}

fn window_options(cx: &mut App) -> WindowOptions {
    let bounds = Bounds::centered(None, size(px(900.), px(640.)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        focus: true,
        titlebar: Some(TitlebarOptions {
            title: Some(SharedString::new(
                format!("{} v{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        ..Default::default()
    }
}
