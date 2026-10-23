#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use gpui_kit::*;
use gpui_kit::assets::{AllAssets, IconName};
use gpui_kit::component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_kit::component::input::{InputState, NumberInput};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::button::{Button, ButtonVariants as _, Toggle};
use gpui_kit::component::{ActiveTheme, Colorize, Icon, IndexPath, Sizable};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::Theme;
use image::RgbaImage;

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
const MAX_CANVAS_SIZE: usize = 4096;
const MAX_HISTORY: usize = 100;

const BACKGROUND: [u8; 4] = [0xff, 0xff, 0xff, 0xff];
// color of the area around the canvas
//const SURROUND: [u8; 3] = [0xc8, 0xc8, 0xc8];


type Rect = (i32, i32, i32, i32); // x0, y0, x1, y1 (end-exclusive)

fn union_rect(a: Rect, b: Rect) -> Rect {
    (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))
}

/// center of a canvas pixel in continuous coordinates
fn pixel_center(p: (i32, i32)) -> (f32, f32) {
    (p.0 as f32 + 0.5, p.1 as f32 + 0.5)
}

/// Antialiasing coverage of the current stroke or shape (0..=255 per pixel).
struct Coverage {
    width: usize,
    height: usize,
    data: Vec<u8>,
}
impl Coverage {
    fn new(width: usize, height: usize) -> Self {
        Self { width, height, data: vec![0; width * height] }
    }

    // clip a floating-point bounding box to the mask
    fn clip(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        (
            (x0.floor() as i32).max(0),
            (y0.floor() as i32).max(0),
            (x1.ceil() as i32).min(self.width as i32),
            (y1.ceil() as i32).min(self.height as i32),
        )
    }

    fn clear_rect(&mut self, rect: Rect) {
        let x0 = rect.0.max(0) as usize;
        let y0 = rect.1.max(0) as usize;
        let x1 = rect.2.clamp(0, self.width as i32) as usize;
        let y1 = rect.3.clamp(0, self.height as i32) as usize;
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for y in y0..y1 {
            self.data[y * self.width + x0..y * self.width + x1].fill(0);
        }
    }

    // keep the larger coverage when shapes overlap, so overlaps never get darker
    fn put(&mut self, x: i32, y: i32, value: f32) {
        let v = (value.clamp(0., 1.) * 255.).round() as u8;
        let i = y as usize * self.width + x as usize;
        if v > self.data[i] {
            self.data[i] = v;
        }
    }

    /// A line segment with round caps; the line width is `2 * radius`.
    fn add_capsule(&mut self, a: (f32, f32), b: (f32, f32), radius: f32) -> Rect {
        let rect = self.clip(
            a.0.min(b.0) - radius - 1.,
            a.1.min(b.1) - radius - 1.,
            a.0.max(b.0) + radius + 1.,
            a.1.max(b.1) + radius + 1.,
        );
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len2 = dx * dx + dy * dy;
        for y in rect.1..rect.3 {
            for x in rect.0..rect.2 {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                // distance from the pixel center to the segment
                let t = if len2 > 0. {
                    (((fx - a.0) * dx + (fy - a.1) * dy) / len2).clamp(0., 1.)
                } else {
                    0.
                };
                let (qx, qy) = (a.0 + t * dx, a.1 + t * dy);
                let d = ((fx - qx).powi(2) + (fy - qy).powi(2)).sqrt();
                self.put(x, y, radius + 0.5 - d);
            }
        }
        rect
    }

    /// Ellipse outline, drawn as a chain of short capsules.
    fn add_ellipse_outline(&mut self, cx: f32, cy: f32, a: f32, b: f32, radius: f32) -> Rect {
        let steps = ((a.max(b) * 6.) as usize).max(24);
        let mut prev = (cx + a, cy);
        let mut rect: Option<Rect> = None;
        for i in 1..=steps {
            let theta = i as f32 / steps as f32 * std::f32::consts::TAU;
            let p = (cx + a * theta.cos(), cy + b * theta.sin());
            let r = self.add_capsule(prev, p, radius);
            rect = Some(rect.map_or(r, |u| union_rect(u, r)));
            prev = p;
        }
        rect.unwrap_or((0, 0, 0, 0))
    }

    /// Filled ellipse; the edge coverage comes from an approximate distance to the boundary.
    fn add_ellipse_fill(&mut self, cx: f32, cy: f32, a: f32, b: f32) -> Rect {
        let (a, b) = (a.max(0.5), b.max(0.5));
        let rect = self.clip(cx - a - 1., cy - b - 1., cx + a + 1., cy + b + 1.);
        for y in rect.1..rect.3 {
            for x in rect.0..rect.2 {
                let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                let f = dx * dx / (a * a) + dy * dy / (b * b);
                let (gx, gy) = (dx / (a * a), dy / (b * b));
                let grad = 2. * (gx * gx + gy * gy).sqrt();
                // signed distance (negative inside)
                let d = if grad > 1e-6 { (f - 1.) / grad } else { -1.0e3 };
                self.put(x, y, 0.5 - d);
            }
        }
        rect
    }
}

/// An opaque RGBA bitmap: the source of truth of the document.
/// It tracks the bounding box of modified pixels so that history entries stay small.
#[derive(Clone)]
struct Bitmap {
    width: usize,
    height: usize,
    pixels: Vec<[u8; 4]>,
    // modified area since the last history entry: (x0, y0, x1, y1), end-exclusive
    dirty: Option<(i32, i32, i32, i32)>,
}

impl Bitmap {
    fn new(width: usize, height: usize, fill: [u8; 4]) -> Self {
        Self { width, height, pixels: vec![fill; width * height], dirty: None }
    }
    fn from(img: RgbaImage) -> Self {
        let (width, height) = (img.width() as usize, img.height() as usize);
        // the canvas is opaque, so flatten transparency onto white
        let pixels = img.pixels()
            .map(|p| {
                let a = p[3] as u32;
                let blend = |c: u8| ((c as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
                [blend(p[0]), blend(p[1]), blend(p[2]), 0xff]
            })
            .collect();
        Self { width, height, pixels, dirty: None }
    }

    // grow the dirty area; the given rectangle must already be clipped to the bitmap
    fn mark(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        self.dirty = Some(match self.dirty {
            Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
            None => (x0, y0, x1, y1),
        });
    }

    fn get(&self, x: i32, y: i32) -> Option<[u8; 4]> {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return None;
        }
        Some(self.pixels[y as usize * self.width + x as usize])
    }

    fn set(&mut self, x: i32, y: i32, color: [u8; 4]) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        self.pixels[y as usize * self.width + x as usize] = color;
        self.mark(x, y, x + 1, y + 1);
    }

    /// Fill a rectangle, clipped to the bitmap.
    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: [u8; 4]) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.width as i32);
        let y1 = (y + h).min(self.height as i32);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for yy in y0..y1 {
            let row = yy as usize * self.width;
            self.pixels[row + x0 as usize..row + x1 as usize].fill(color);
        }
        self.mark(x0, y0, x1, y1);
    }

    /// Paint a square brush of `size` pixels centered on (cx, cy).
    fn stamp(&mut self, cx: i32, cy: i32, size: i32, color: [u8; 4]) {
        self.fill_rect(cx - size / 2, cy - size / 2, size, size, color);
    }

    /// Draw a line with Bresenham's algorithm, stamping the brush at every step.
    fn line(&mut self, from: (i32, i32), to: (i32, i32), size: i32, color: [u8; 4]) {
        let (mut x0, mut y0) = from;
        let (x1, y1) = to;
        let dx = (x1 - x0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let dy = -(y1 - y0).abs();
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            self.stamp(x0, y0, size, color);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    /// Rectangle between two inclusive corners.
    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, size: i32, filled: bool, color: [u8; 4]) {
        if filled {
            self.fill_rect(x0, y0, x1 - x0 + 1, y1 - y0 + 1, color);
            return;
        }
        self.line((x0, y0), (x1, y0), size, color);
        self.line((x1, y0), (x1, y1), size, color);
        self.line((x1, y1), (x0, y1), size, color);
        self.line((x0, y1), (x0, y0), size, color);
    }

    /// Ellipse inscribed in the rectangle between two inclusive corners.
    fn ellipse(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, size: i32, filled: bool, color: [u8; 4]) {
        let cx = (x0 + x1) as f32 / 2.;
        let cy = (y0 + y1) as f32 / 2.;
        let a  = (x1 - x0) as f32 / 2.;
        let b  = (y1 - y0) as f32 / 2.;

        if filled {
            // one span per row
            for y in y0..=y1 {
                let t = if b > 0. { (y as f32 - cy) / b } else { 0. };
                let half = a * (1. - t * t).max(0.).sqrt();
                let left = (cx - half).round() as i32;
                let right = (cx + half).round() as i32;
                self.fill_rect(left, y, right - left + 1, 1, color);
            }
            return;
        }

        // sample the outline densely and connect the samples with lines, so no gaps remain
        let steps = ((a.max(b) * 4.) as usize).max(16);
        let mut prev: Option<(i32, i32)> = None;
        for i in 0..=steps {
            let theta = i as f32 / steps as f32 * std::f32::consts::TAU;
            let p = ((cx + a * theta.cos()).round() as i32, (cy + b * theta.sin()).round() as i32);
            match prev {
                Some(q) if q != p => self.line(q, p, size, color),
                None => self.stamp(p.0, p.1, size, color),
                _ => {}
            }
            prev = Some(p);
        }
    }

    /// Scanline flood fill (4-connected) that replaces the region of the clicked color.
    /// With `smooth_edges`, the antialiased pixels around the region are recolored too.
    fn flood_fill(&mut self, x: i32, y: i32, color: [u8; 4], smooth_edges: bool) {
        let Some(target) = self.get(x, y) else { return };
        if target == color {
            return;
        }

        let w = self.width;
        // `filled` marks the region; its pixels are painted after the search finishes,
        // so all reads below still see the original colors
        let mut filled = vec![false; w * self.height];
        let mut spans: Vec<(i32, i32, i32)> = Vec::new(); // (y, left, right), inclusive

        let mut stack = vec![(x, y)];
        while let Some((x, y)) = stack.pop() {
            if self.get(x, y) != Some(target) || filled[y as usize * w + x as usize] {
                continue;
            }
            // extend the span to the left and right
            let mut left = x;
            while self.get(left - 1, y) == Some(target) {
                left -= 1;
            }
            let mut right = x;
            while self.get(right + 1, y) == Some(target) {
                right += 1;
            }
            for i in left..=right {
                filled[y as usize * w + i as usize] = true;
            }
            spans.push((y, left, right));

            // queue one seed per run of target pixels in the rows above and below
            for ny in [y - 1, y + 1] {
                let mut inside = false;
                for i in left..=right {
                    let matches = self.get(i, ny) == Some(target) && !filled[ny as usize * w + i as usize];
                    if matches && !inside {
                        stack.push((i, ny));
                    }
                    inside = matches;
                }
            }
        }

        // antialiased pixels touching the region (8-neighborhood) get their share of the
        // old color replaced, instead of being left behind as a halo
        let mut edges: Vec<(i32, i32, [u8; 4])> = Vec::new();
        if smooth_edges {
            let height = self.height as i32;
            let mut seen = filled.clone();
            for &(y, left, right) in &spans {
                for ny in y - 1..=y + 1 {
                    for nx in left - 1..=right + 1 {
                        if nx < 0 || ny < 0 || nx >= w as i32 || ny >= height {
                            continue;
                        }
                        let i = ny as usize * w + nx as usize;
                        if seen[i] {
                            continue;
                        }
                        seen[i] = true;
                        if let Some(c) = self.blend_edge(nx, ny, target, color) {
                            edges.push((nx, ny, c));
                        }
                    }
                }
            }
        }

        for &(y, left, right) in &spans {
            self.fill_rect(left, y, right - left + 1, 1, color);
        }
        for (x, y, c) in edges {
            self.set(x, y, c);
        }
    }

    /// New color of an antialiased pixel next to a filled region, or `None` to keep it.
    /// The pixel is modeled as a mix of the old region color `target` and a line color;
    /// only the `target` share of the mix is replaced by `color`.
    fn blend_edge(&self, x: i32, y: i32, target: [u8; 4], color: [u8; 4]) -> Option<[u8; 4]> {
        let c = self.get(x, y)?;
        if c == target {
            return None; // not connected to the region, so leave it alone
        }
        let dist2 = |p: [u8; 4]| -> i32 {
            (0..3).map(|k| (p[k] as i32 - target[k] as i32).pow(2)).sum()
        };
        // the most distant color nearby stands in for the line color
        let mut line = c;
        for dy in -1..=1 {
            for dx in -1..=1 {
                if let Some(n) = self.get(x + dx, y + dy) {
                    if dist2(n) > dist2(line) {
                        line = n;
                    }
                }
            }
        }
        let d: [f32; 3] = std::array::from_fn(|k| line[k] as f32 - target[k] as f32);
        let len2: f32 = d.iter().map(|v| v * v).sum();
        // share of the line color in this pixel (1.0 = solid line)
        let a = ((0..3)
            .map(|k| (c[k] as f32 - target[k] as f32) * d[k])
            .sum::<f32>()
            / len2)
            .clamp(0., 1.);
        let mut out = c;
        for k in 0..3 {
            let v = c[k] as f32 + (1. - a) * (color[k] as f32 - target[k] as f32);
            out[k] = v.round().clamp(0., 255.) as u8;
        }
        Some(out)
    }

    /// Copy a rectangle into a new bitmap (pixels outside of this bitmap become background).
    fn copy_region(&self, x: i32, y: i32, w: i32, h: i32) -> Bitmap {
        let mut out = Bitmap::new(w.max(1) as usize, h.max(1) as usize, BACKGROUND);
        for yy in 0..h {
            for xx in 0..w {
                if let Some(p) = self.get(x + xx, y + yy) {
                    out.pixels[yy as usize * out.width + xx as usize] = p;
                }
            }
        }
        out
    }

    /// Overwrite a part of this bitmap with `src` placed at (x, y), clipped.
    fn paste(&mut self, src: &Bitmap, x: i32, y: i32) {
        for sy in 0..src.height as i32 {
            let dy = y + sy;
            if dy < 0 || dy >= self.height as i32 {
                continue;
            }
            for sx in 0..src.width as i32 {
                let dx = x + sx;
                if dx < 0 || dx >= self.width as i32 {
                    continue;
                }
                self.pixels[dy as usize * self.width + dx as usize] =
                    src.pixels[sy as usize * src.width + sx as usize];
            }
        }
        self.mark(
            x.max(0),
            y.max(0),
            (x + src.width as i32).min(self.width as i32),
            (y + src.height as i32).min(self.height as i32),
        );
    }

    /// Restore a rectangle from `src` (a bitmap of the same size), clipped.
    fn restore_region(&mut self, src: &Bitmap, rect: (i32, i32, i32, i32)) {
        let x0 = rect.0.max(0);
        let y0 = rect.1.max(0);
        let x1 = rect.2.min(self.width as i32);
        let y1 = rect.3.min(self.height as i32);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for y in y0..y1 {
            let row = y as usize * self.width;
            let range = row + x0 as usize..row + x1 as usize;
            self.pixels[range.clone()].copy_from_slice(&src.pixels[range]);
        }
        self.mark(x0, y0, x1, y1);
    }

    /// Set the pixels of `rect` to the backup mixed with `color` by the coverage.
    fn composite(&mut self, backup: &Bitmap, cov: &Coverage, color: [u8; 4], rect: Rect) {
        let x0 = rect.0.max(0);
        let y0 = rect.1.max(0);
        let x1 = rect.2.min(self.width as i32);
        let y1 = rect.3.min(self.height as i32);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let mix = |from: u8, to: u8, c: u32| ((from as u32 * (255 - c) + to as u32 * c + 127) / 255) as u8;
        for y in y0..y1 {
            let row = y as usize * self.width;
            for x in x0..x1 {
                let i = row + x as usize;
                let c = cov.data[i] as u32;
                let b = backup.pixels[i];
                self.pixels[i] = match c {
                    0 => b,
                    255 => color,
                    _ => [mix(b[0], color[0], c), mix(b[1], color[1], c), mix(b[2], color[2], c), 0xff],
                };
            }
        }
        self.mark(x0, y0, x1, y1);
    }

    // history helpers: these do not touch the dirty area
    fn read_region(&self, x: usize, y: usize, w: usize, h: usize) -> Vec<[u8; 4]> {
        let mut out = Vec::with_capacity(w * h);
        for yy in y..y + h {
            let row = yy * self.width;
            out.extend_from_slice(&self.pixels[row + x..row + x + w]);
        }
        out
    }

    fn write_region(&mut self, x: usize, y: usize, w: usize, h: usize, data: &[[u8; 4]]) {
        for (i, yy) in (y..y + h).enumerate() {
            let row = yy * self.width;
            self.pixels[row + x..row + x + w].copy_from_slice(&data[i * w..(i + 1) * w]);
        }
    }

}

/// One undo step. A patch stores only the changed rectangle.
enum Edit {
    Patch {
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        before: Vec<[u8; 4]>,
        after: Vec<[u8; 4]>,
    },
    // used when the canvas size changes
    Replace {
        before: Bitmap,
        after: Bitmap,
    },
}

/// A rectangular selection. The pixels are lifted ("floating") when the selection is first moved.
struct Selection {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    floating: Option<Bitmap>,
}
impl Selection {
    fn contains(&self, p: (i32, i32)) -> bool {
        p.0 >= self.x && p.0 < self.x + self.w && p.1 >= self.y && p.1 < self.y + self.h
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Pencil,
    Eraser,
    Fill,
    Picker,
    Line,
    Rectangle,
    Oval,
    Select,
}

/// What the current mouse drag is doing.
#[derive(Clone, Copy)]
enum Drag {
    None,
    Draw,
    Shape { start: (i32, i32) },
    SelectNew { start: (i32, i32) },
    SelectMove { grab: (i32, i32) },
    // `grab` is the offset (in logical pixels) between the pointer and the thumb start
    ScrollbarX { grab: f32 },
    ScrollbarY { grab: f32 },
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

pub struct Application {
    bitmap: Bitmap,
    // the bitmap as of the last history entry, used to build patches
    committed: Bitmap,
    undo_stack: Vec<Edit>,
    redo_stack: Vec<Edit>,
    // undo depth of the saved (or freshly opened) document state;
    // None when that state can no longer be reached with undo / redo
    saved_depth: Option<usize>,
    // file the document was opened from or last saved to (None = Untitled)
    file_path: Option<PathBuf>,
    tool: Tool,
    drag: Drag,
    // last pixel touched while dragging (canvas coordinates)
    last_pos: (i32, i32),
    brush_size: i32,
    shape_filled: bool,
    // anti-aliased drawing on/off, and the coverage of the stroke in progress (AA only)
    antialias: bool,
    coverage: Option<Coverage>,
    // shape preview: the bitmap before the shape and the area the preview covers
    shape_backup: Option<Bitmap>,
    shape_bounds: Option<(i32, i32, i32, i32)>,
    selection: Option<Selection>,
    // whether the mouse pointer is over the selection (decides the pointer shape)
    over_selection: bool,    // pixel under the mouse pointer, if it is inside the canvas
    cursor_px: Option<(i32, i32)>,
    current_color: Hsla,
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
    // the visible part of the bitmap, already magnified (device pixel resolution)
    view_image: Option<Arc<RenderImage>>,
    view_pixels: (usize, usize),
    view_dirty: bool,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>, // keep subscriptions alive for the lifetime of the view
}

impl Application {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {

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
                        this.brush_size = v.clamp(1, 500);
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

        let initial_color: Hsla = rgb(PALETTE[0]).into();
        let color_picker = cx.new(|cx| ColorPickerState::new(window, cx).default_value(initial_color));

        // reflect colors chosen in the picker popover into the drawing color
        let color_sub = cx.subscribe(&color_picker, |this, _, ev, cx| match ev {
            ColorPickerEvent::Change(color) => {
                if let Some(color) = color {
                    this.current_color = *color;
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

        let bitmap = Bitmap::new(DEFAULT_CANVAS_SIZE.0, DEFAULT_CANVAS_SIZE.1, BACKGROUND);

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
            this.update(cx, |app, cx| app.confirm_discard(cx)).unwrap_or(true)
        });

        let app = Self {
            committed: bitmap.clone(),
            bitmap,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            saved_depth: Some(0),
            file_path: None,
            tool: Tool::Pencil,
            drag: Drag::None,
            last_pos: (0, 0),
            brush_size: 3,
            shape_filled: false,
            antialias: false,
            coverage: None,
            shape_backup: None,
            shape_bounds: None,
            selection: None,
            over_selection: false,
            cursor_px: None,
            current_color: initial_color,
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
            focus_handle: cx.focus_handle(),
            _subscriptions: vec![color_sub, size_sub, zoom_sub, appearance_sub],
        };
        // give the canvas the keyboard focus at startup so that the shortcuts work at once
        window.focus(&app.focus_handle, cx);
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
            (self.bitmap.width as f32 * zoom + CANVAS_MARGIN * 2.).max(view_w),
            (self.bitmap.height as f32 * zoom + CANVAS_MARGIN * 2.).max(view_h),
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
            (content_w - self.bitmap.width as f32 * zoom) / 2. - self.scroll.0,
            (content_h - self.bitmap.height as f32 * zoom) / 2. - self.scroll.1,
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
        self.scroll.0 = (content_w - self.bitmap.width as f32 * new_zoom) / 2. - (anchor.0 - cx * new_zoom);
        self.scroll.1 = (content_h - self.bitmap.height as f32 * new_zoom) / 2. - (anchor.1 - cy * new_zoom);
        self.clamp_scroll();
        self.mark_dirty();
    }

    // zoom so that the whole canvas fits into the viewport
    fn fit_to_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let avail_w = f32::from(self.viewport_size.width) - CANVAS_MARGIN * 2.;
        let avail_h = f32::from(self.viewport_size.height) - CANVAS_MARGIN * 2.;
        let zoom = (avail_w / self.bitmap.width as f32).min(avail_h / self.bitmap.height as f32);
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

    // convert a window position into canvas pixel coordinates (maybe outside the bitmap)
    fn to_canvas_px(&self, pos: Point<Pixels>) -> (i32, i32) {
        let origin = self.viewport_origin.get();
        let (ox, oy) = self.canvas_origin();
        let zoom = self.zoom();
        let x = (f32::from(pos.x - origin.x) - ox) / zoom;
        let y = (f32::from(pos.y - origin.y) - oy) / zoom;
        (x.floor() as i32, y.floor() as i32)
    }

    // -- view rendering ------------------------------------------------------

    // build the visible part of the bitmap, magnified with nearest-neighbor sampling
    fn rebuild_view_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let scale = window.scale_factor();
        let w = (f32::from(self.viewport_size.width) * scale).ceil().max(1.) as usize;
        let h = (f32::from(self.viewport_size.height) * scale).ceil().max(1.) as usize;

        // device pixels per canvas pixel, and the canvas origin in device pixels
        let zoom = self.zoom() * scale;
        let (ox, oy) = self.canvas_origin();
        let (ox, oy) = (ox * scale, oy * scale);

        let bw = self.bitmap.width as i32;
        let bh = self.bitmap.height as i32;

        // canvas coordinate of every device column/row (-1 = outside the canvas)
        let map = |origin: f32, count: usize, limit: i32| -> Vec<i32> {
            (0..count)
                .map(|i| {
                    let c = ((i as f32 + 0.5 - origin) / zoom).floor() as i32;
                    if c < 0 || c >= limit { -1 } else { c }
                })
                .collect()
        };
        let xs = map(ox, w, bw);
        let ys = map(oy, h, bh);

        // lifted selection pixels are drawn on top of the bitmap
        let floating = self
            .selection
            .as_ref()
            .and_then(|s| s.floating.as_ref().map(|f| (s.x, s.y, f)));

        let grid = self.zoom() >= GRID_MIN_ZOOM;
        let mut buf = vec![0u8; w * h * 4];
        for y in 0..h {
            let cry = ys[y];
            // first device row of a canvas pixel row
            let on_h_line = grid && cry >= 0 && (y == 0 || ys[y - 1] != cry);
            let row = &mut buf[y * w * 4..(y + 1) * w * 4];
            for x in 0..w {
                let crx = xs[x];
                let [r, g, b] = if crx < 0 || cry < 0 {
                    to_rgb8(invert_lightness(cx.theme().muted_foreground))
                } else {
                    let p = match floating {
                        Some((sx, sy, f))
                        if crx >= sx && crx < sx + f.width as i32 && cry >= sy && cry < sy + f.height as i32 =>
                            {
                                f.pixels[(cry - sy) as usize * f.width + (crx - sx) as usize]
                            }
                        _ => self.bitmap.pixels[cry as usize * self.bitmap.width + crx as usize],
                    };
                    let mut c = [p[0], p[1], p[2]];
                    // first device column of a canvas pixel column
                    if grid && (on_h_line || x == 0 || xs[x - 1] != crx) {
                        for v in &mut c {
                            *v = ((*v as u16 + 0x80) / 2) as u8;
                        }
                    }
                    c
                };
                // gpui expects BGRA
                row[x * 4] = b;
                row[x * 4 + 1] = g;
                row[x * 4 + 2] = r;
                row[x * 4 + 3] = 0xff;
            }
        }

        // selection outline: a dashed black/white rectangle, one device pixel wide
        if let Some(sel) = &self.selection {
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

    /// true when the document differs from the saved state
    fn is_modified(&self) -> bool {
        // pixels that were lifted by a selection move are not in the history yet
        let lifted = self.selection.as_ref().is_some_and(|s| s.floating.is_some());
        lifted || self.saved_depth != Some(self.undo_stack.len())
    }

    fn push_edit(&mut self, edit: Edit) {
        // a new edit discards the redo branch; if the saved state was inside it, it is lost
        if self.saved_depth.is_some_and(|d| d > self.undo_stack.len()) {
            self.saved_depth = None;
        }
        self.undo_stack.push(edit);
        self.redo_stack.clear();
        if self.undo_stack.len() > MAX_HISTORY {
            self.undo_stack.remove(0);
            // every depth moves down by one; the saved state is lost if it was the oldest one
            self.saved_depth = self.saved_depth.and_then(|d| d.checked_sub(1));
        }
    }

    // turn the pixels modified since the last entry into one undo step
    fn record_history(&mut self) {
        let Some((x0, y0, x1, y1)) = self.bitmap.dirty.take() else { return };
        let (x, y, w, h) = (x0 as usize, y0 as usize, (x1 - x0) as usize, (y1 - y0) as usize);
        let before = self.committed.read_region(x, y, w, h);
        let after = self.bitmap.read_region(x, y, w, h);
        if before == after {
            return; // nothing actually changed
        }
        self.committed.write_region(x, y, w, h, &after);
        self.push_edit(Edit::Patch { x, y, w, h, before, after });
    }

    fn apply_edit(&mut self, edit: &Edit, undo: bool) {
        match edit {
            Edit::Patch { x, y, w, h, before, after } => {
                let data = if undo { before } else { after };
                self.bitmap.write_region(*x, *y, *w, *h, data);
                self.committed.write_region(*x, *y, *w, *h, data);
            }
            Edit::Replace { before, after } => {
                let target = if undo { before } else { after };
                self.bitmap = target.clone();
                self.committed = target.clone();
            }
        }
        self.bitmap.dirty = None;
    }

    fn after_history_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_size_inputs(window, cx);
        self.clamp_scroll();
        self.mark_dirty();
        cx.notify();
    }

    fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // flatten a floating selection first, so that undo cancels the move
        self.commit_selection();
        if let Some(edit) = self.undo_stack.pop() {
            self.apply_edit(&edit, true);
            self.redo_stack.push(edit);
            self.after_history_change(window, cx);
        }
    }

    fn redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_selection();
        if let Some(edit) = self.redo_stack.pop() {
            self.apply_edit(&edit, false);
            self.undo_stack.push(edit);
            self.after_history_change(window, cx);
        }
    }

    // replace the whole bitmap (resize / open); `record` makes it undoable
    fn replace_bitmap(&mut self, new_bitmap: Bitmap, record: bool) {
        self.selection = None;
        self.drag = Drag::None;
        if record {
            self.record_history();
            let before = std::mem::replace(&mut self.committed, new_bitmap.clone());
            self.push_edit(Edit::Replace { before, after: new_bitmap.clone() });
        } else {
            self.committed = new_bitmap.clone();
            self.undo_stack.clear();
            self.redo_stack.clear();
            self.saved_depth = Some(0);
        }
        self.bitmap = new_bitmap;
    }

    // -- selection -----------------------------------------------------------

    // flatten the selection into the bitmap (a no-op if the pixels were never lifted)
    fn commit_selection(&mut self) {
        let Some(sel) = self.selection.take() else { return };
        if let Some(floating) = sel.floating {
            self.bitmap.paste(&floating, sel.x, sel.y);
            self.record_history();
        }
        self.mark_dirty();
    }

    fn delete_selection(&mut self, cx: &mut Context<Self>) {
        let Some(sel) = self.selection.take() else { return };
        // lifted pixels already left a hole behind, so only an unlifted selection needs clearing
        if sel.floating.is_none() {
            self.bitmap.fill_rect(sel.x, sel.y, sel.w, sel.h, BACKGROUND);
        }
        self.record_history();
        self.mark_dirty();
        cx.notify();
    }

    // Esc: drop the selection, keeping the moved pixels in place
    fn deselect(&mut self, cx: &mut Context<Self>) {
        if matches!(self.drag, Drag::SelectNew { .. } | Drag::SelectMove { .. }) {
            self.drag = Drag::None;
        }
        self.commit_selection();
        self.over_selection = false;
        self.mark_dirty();
        cx.notify();
    }

    fn update_selection(&mut self, start: (i32, i32), end: (i32, i32)) {
        let max_x = self.bitmap.width as i32 - 1;
        let max_y = self.bitmap.height as i32 - 1;
        let clamp = |p: (i32, i32)| (p.0.clamp(0, max_x), p.1.clamp(0, max_y));
        let (s, e) = (clamp(start), clamp(end));
        self.selection = Some(Selection {
            x: s.0.min(e.0),
            y: s.1.min(e.1),
            w: (s.0 - e.0).abs() + 1,
            h: (s.1 - e.1).abs() + 1,
            floating: None,
        });
    }

    // copy the selection to the system clipboard as an image
    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let Some(sel) = &self.selection else { return };
        // lifted pixels live in the selection, otherwise read them from the bitmap
        let region = match &sel.floating {
            Some(floating) => floating.clone(),
            None => self.bitmap.copy_region(sel.x, sel.y, sel.w, sel.h),
        };
        let bytes: Vec<u8> = region.pixels.iter().flatten().copied().collect();
        let Some(buffer) = image::RgbaImage::from_raw(region.width as u32, region.height as u32, bytes) else {
            return;
        };

        // encode as PNG and put it on the clipboard through gpui
        let mut png = Vec::new();
        if let Err(e) = buffer.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png) {
            show_error(&format!("Failed to encode the image: {e}"));
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(ImageFormat::Png, png)));
    }

    // paste the clipboard image as a floating selection that can be moved before it is committed
    fn paste_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else { return };
        // the clipboard holds no image (for example text): nothing to paste
        let Some(clip_image) = item.entries().iter().find_map(|entry| match entry {
            ClipboardEntry::Image(image) => Some(image),
            _ => None,
        }) else {
            return;
        };
        let decoded = match image::load_from_memory(&clip_image.bytes) {
            Ok(img) => img.to_rgba8(),
            Err(e) => {
                show_error(&format!("Failed to read the clipboard image: {e}"));
                return;
            }
        };
        let (w, h) = (decoded.width() as usize, decoded.height() as usize);
        if w == 0 || h == 0 || w > MAX_CANVAS_SIZE || h > MAX_CANVAS_SIZE {
            show_error(&format!("The clipboard image must be at most {MAX_CANVAS_SIZE} px on each side."));
            return;
        }
        let pasted = Bitmap::from(decoded);

        // flatten the previous selection first
        self.commit_selection();

        // grow the canvas when the image does not fit (undoable)
        if w > self.bitmap.width || h > self.bitmap.height {
            let (old_w, old_h) = (self.bitmap.width, self.bitmap.height);
            let mut grown = Bitmap::new(w.max(old_w), h.max(old_h), BACKGROUND);
            let data = self.bitmap.read_region(0, 0, old_w, old_h);
            grown.write_region(0, 0, old_w, old_h, &data);
            self.replace_bitmap(grown, true);
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
        let (bw, bh) = (self.bitmap.width as i32, self.bitmap.height as i32);
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

        self.tool = Tool::Select;
        self.selection = Some(Selection { x, y, w: w as i32, h: h as i32, floating: Some(pasted) });
        self.mark_dirty();
        cx.notify();
    }

    fn select_mouse_down(&mut self, p: (i32, i32)) {
        let inside = self.selection.as_ref().is_some_and(|s| s.contains(p));
        if inside {
            let sel = self.selection.as_mut().unwrap();
            // lift the pixels, leaving a background-colored hole
            if sel.floating.is_none() {
                sel.floating = Some(self.bitmap.copy_region(sel.x, sel.y, sel.w, sel.h));
                self.bitmap.fill_rect(sel.x, sel.y, sel.w, sel.h, BACKGROUND);
            }
            self.drag = Drag::SelectMove { grab: (p.0 - sel.x, p.1 - sel.y) };
        } else {
            self.commit_selection();
            self.drag = Drag::SelectNew { start: p };
        }
    }

    // -- drawing -------------------------------------------------------------

    // color used by the pencil-like tools
    fn tool_color(&self) -> [u8; 4] {
        match self.tool {
            Tool::Eraser => BACKGROUND,
            _ => to_rgba8(self.current_color),
        }
    }

    // draw (or redraw) the shape preview between two canvas pixels
    fn draw_shape(&mut self, start: (i32, i32), end: (i32, i32)) {
        let Some(backup) = self.shape_backup.as_ref() else { return };
        // erase the previous preview
        if let Some(bounds) = self.shape_bounds {
            self.bitmap.restore_region(backup, bounds);
            if let Some(cov) = self.coverage.as_mut() {
                cov.clear_rect(bounds);
            }
        }

        let color = to_rgba8(self.current_color);
        let size = self.brush_size;
        let (x0, x1) = (start.0.min(end.0), start.0.max(end.0));
        let (y0, y1) = (start.1.min(end.1), start.1.max(end.1));
        if let Some(cov) = self.coverage.as_mut() {
            // anti-aliased: build the coverage, then composite it over the original pixels
            let r = size as f32 / 2.;
            let touched: Option<Rect> = match self.tool {
                Tool::Line => Some(cov.add_capsule(pixel_center(start), pixel_center(end), r)),
                // a filled rectangle is pixel-aligned, so it needs no smoothing
                Tool::Rectangle if self.shape_filled => {
                    self.bitmap.rect(x0, y0, x1, y1, size, true, color);
                    None
                }
                Tool::Rectangle => {
                    let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
                    let mut rect = cov.add_capsule(pixel_center(corners[0]), pixel_center(corners[1]), r);
                    for i in 1..4 {
                        let side = cov.add_capsule(
                            pixel_center(corners[i]),
                            pixel_center(corners[(i + 1) % 4]),
                            r,
                        );
                        rect = union_rect(rect, side);
                    }
                    Some(rect)
                }
                Tool::Oval => {
                    let cx = (x0 + x1) as f32 / 2. + 0.5;
                    let cy = (y0 + y1) as f32 / 2. + 0.5;
                    let a = (x1 - x0) as f32 / 2.;
                    let b = (y1 - y0) as f32 / 2.;
                    if self.shape_filled {
                        Some(cov.add_ellipse_fill(cx, cy, a + 0.5, b + 0.5))
                    } else {
                        Some(cov.add_ellipse_outline(cx, cy, a, b, r))
                    }
                }
                _ => None,
            };
            if let Some(rect) = touched {
                self.bitmap.composite(backup, cov, color, rect);
            }
        } else {
            match self.tool {
                Tool::Line => self.bitmap.line(start, end, size, color),
                Tool::Rectangle => self.bitmap.rect(x0, y0, x1, y1, size, self.shape_filled, color),
                Tool::Oval => self.bitmap.ellipse(x0, y0, x1, y1, size, self.shape_filled, color),
                _ => {}
            }
        }
        // the preview may extend beyond the corners by the brush size
        self.shape_bounds = Some((x0 - size, y0 - size, x1 + size + 1, y1 + size + 1));
    }

    // add a round-capped segment to the stroke's coverage and composite it over the saved pixels
    fn aa_segment(&mut self, from: (i32, i32), to: (i32, i32), color: [u8; 4]) {
        let (Some(backup), Some(cov)) = (self.shape_backup.as_ref(), self.coverage.as_mut()) else {
            return;
        };
        let radius = self.brush_size as f32 / 2.;
        let rect = cov.add_capsule(pixel_center(from), pixel_center(to), radius);
        self.bitmap.composite(backup, cov, color, rect);
    }

    // -- canvas size ---------------------------------------------------------

    fn sync_size_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (width, height) = (self.bitmap.width, self.bitmap.height);
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
            show_error("Enter the width and height as whole numbers.");
            return false;
        };
        if width == 0 || height == 0 || width > MAX_CANVAS_SIZE || height > MAX_CANVAS_SIZE {
            show_error(&format!("The size must be between 1 and {MAX_CANVAS_SIZE}."));
            return false;
        }
        // the trim origin is the top-left corner of the selection (the canvas origin if nothing is selected);
        // it is read before the selection is flattened, and only after validation succeeded,
        // so that a rejected input leaves the selection intact for the next try
        let (ox, oy) = self.selection.as_ref().map_or((0, 0), |s| (s.x, s.y));
        if (ox, oy) == (0, 0) && width == self.bitmap.width && height == self.bitmap.height {
            return false;
        }

        // flatten a floating selection so that its pixels are part of the trimmed image
        self.commit_selection();

        // trim at (ox, oy); the part outside of the current canvas becomes the background
        let new_bitmap = self.bitmap.copy_region(ox, oy, width as i32, height as i32);

        self.replace_bitmap(new_bitmap, true);
        self.after_history_change(window, cx);
        true
    }

    fn open_resize_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // show the selection size in the inputs, or the canvas size when nothing is selected
        let trim = self.selection.is_some();
        match self.selection.as_ref().map(|s| (s.w as usize, s.h as usize)) {
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
            dialog
                .title(if trim { "Trim to selection" } else { "Resize canvas" })
                .w(px(320.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(row("Width", &width_input))
                        .child(row("Height", &height_input))
                        .child(div().text_xs().child(if trim {
                            format!(
                                "1 to {MAX_CANVAS_SIZE} px. The image is trimmed from the top-left of the selection; \
                                 areas outside the image are filled with the background color."
                            )
                        } else {
                            format!("1 to {MAX_CANVAS_SIZE} px. The image stays anchored at the top-left.")
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
                                    let applied = app.update(cx, |this, cx| this.apply_resize(window, cx));
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
        self.focus_canvas(window, cx);
        let p = self.to_canvas_px(event.position);
        let tool = self.tool;
        // every tool except Select flattens an active selection first
        if tool != Tool::Select {
            self.commit_selection();
        }
        match tool {
            Tool::Pencil | Tool::Eraser => {
                self.drag = Drag::Draw;
                self.last_pos = p;
                let color = self.tool_color();
                if self.antialias {
                    // keep the pixels from before the stroke; the stroke is composited over them
                    self.shape_backup = Some(self.bitmap.clone());
                    self.coverage = Some(Coverage::new(self.bitmap.width, self.bitmap.height));
                    self.aa_segment(p, p, color);
                } else {
                    self.bitmap.stamp(p.0, p.1, self.brush_size, color);
                }
            }
            Tool::Fill => {
                let color = to_rgba8(self.current_color);
                self.bitmap.flood_fill(p.0, p.1, color, self.antialias);
                self.record_history();
            }
            Tool::Picker => {
                if let Some(c) = self.bitmap.get(p.0, p.1) {
                    let color: Hsla = rgb(u32::from_be_bytes([0, c[0], c[1], c[2]])).into();
                    self.current_color = color;
                    // keep the picker's displayed color in sync (does not emit Change)
                    self.color_picker
                        .update(cx, |state, cx| state.set_value(color, window, cx));
                    // like classic Paint, go back to drawing after picking
                    self.tool = Tool::Pencil;
                }
            }
            Tool::Line | Tool::Rectangle | Tool::Oval => {
                self.drag = Drag::Shape { start: p };
                self.last_pos = p;
                self.shape_backup = Some(self.bitmap.clone());
                self.coverage = self
                    .antialias
                    .then(|| Coverage::new(self.bitmap.width, self.bitmap.height));
                self.shape_bounds = None;
                self.draw_shape(p, p);
            }
            Tool::Select => self.select_mouse_down(p),
        }
        self.mark_dirty();
        cx.notify();
    }

    fn handle_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let p = self.to_canvas_px(event.position);

        // cursor position shown in the footer
        let inside = p.0 >= 0 && p.1 >= 0 && p.0 < self.bitmap.width as i32 && p.1 < self.bitmap.height as i32;
        let cursor = inside.then_some(p);
        let mut changed = false;
        if cursor != self.cursor_px {
            self.cursor_px = cursor;
            changed = true;
        }
        // the pointer shape depends on whether the pointer is over the selection
        let over = self.selection.as_ref().is_some_and(|s| s.contains(p));
        if over != self.over_selection {
            self.over_selection = over;
            changed = true;
        }
        match self.drag {
            Drag::None => {}
            Drag::Draw => {
                if p != self.last_pos {
                    let color = self.tool_color();
                    if self.coverage.is_some() {
                        self.aa_segment(self.last_pos, p, color);
                    } else {
                        self.bitmap.line(self.last_pos, p, self.brush_size, color);
                    }
                    self.last_pos = p;
                    self.mark_dirty();
                    changed = true;
                }
            }
            Drag::Shape { start } => {
                if p != self.last_pos {
                    self.last_pos = p;
                    self.draw_shape(start, p);
                    self.mark_dirty();
                    changed = true;
                }
            }
            Drag::SelectNew { start } => {
                self.update_selection(start, p);
                self.mark_dirty();
                changed = true;
            }
            Drag::SelectMove { grab } => {
                if let Some(sel) = self.selection.as_mut() {
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
        }
        if changed {
            cx.notify();
        }
    }

    fn handle_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::Draw => {
                self.shape_backup = None;
                self.coverage = None;
                self.record_history();
            }
            Drag::Shape { .. } => {
                self.shape_backup = None;
                self.shape_bounds = None;
                self.coverage = None;
                self.record_history();
            }
            Drag::SelectNew { .. } => {
                // a plain click (no drag) just clears the selection
                if self.selection.as_ref().is_some_and(|s| s.w == 1 && s.h == 1) {
                    self.selection = None;
                }
            }
            // the move is recorded as one step when the selection is flattened
            Drag::SelectMove { .. }
            | Drag::ScrollbarX { .. }
            | Drag::ScrollbarY { .. }
            | Drag::None => {}
        }

        // the selection may have just been created, moved or cleared
        let p = self.to_canvas_px(event.position);
        self.over_selection = self.selection.as_ref().is_some_and(|s| s.contains(p));

        self.mark_dirty();
        cx.notify();
    }

    // a file was dropped from the OS onto the viewport
    fn handle_drop(&mut self, paths: &ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        // open the first dropped file; the others are ignored
        if let Some(path) = paths.paths().first() {
            self.load_image(path, window, cx);
            self.focus_canvas(window, cx);
        }
    }

    fn handle_scroll_wheel(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {

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

        // the control keep their own keyboard handling
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
                "c" => self.copy_selection(cx),
                "v" => self.paste_image(window, cx),
                "s" => { self.save_png(key.modifiers.shift, cx); }
                "n" => self.new_document(window, cx),
                "o" => self.open_png(window, cx),
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

    // -- files ---------------------------------------------------------------

    // returns true when the image was saved;
    // `save_as` forces the file dialog even if the document already has a path
    fn save_png(&mut self, save_as: bool, cx: &mut Context<Self>) -> bool {
        // make sure a moved selection is part of the saved image
        self.commit_selection();
        self.drag = Drag::None;

        // overwrite the current file directly, but only when it is a PNG
        let existing = self.file_path.clone().filter(|p| {
            !save_as && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png"))
        });
        let path = match existing {
            Some(path) => path,
            None => {
                let mut dialog = rfd::FileDialog::new().add_filter("PNG image", &["png"]);
                // start from the current file's folder and name (with a .png extension)
                dialog = match &self.file_path {
                    Some(p) => {
                        let name = p
                            .file_stem()
                            .map(|s| format!("{}.png", s.to_string_lossy()))
                            .unwrap_or_else(|| "drawing.png".to_string());
                        if let Some(dir) = p.parent() {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.set_file_name(name)
                    }
                    None => dialog.set_file_name("drawing.png"),
                };
                let Some(path) = dialog.save_file() else {
                    return false; // canceled
                };
                if path.extension().is_none() { path.with_extension("png") } else { path }
            }
        };

        let data: Vec<u8> = self.bitmap.pixels.iter().flatten().copied().collect();
        let Some(img) = image::RgbaImage::from_raw(self.bitmap.width as u32, self.bitmap.height as u32, data) else {
            show_error("Failed to build the image.");
            return false;
        };
        if let Err(e) = img.save_with_format(&path, image::ImageFormat::Png) {
            show_error(&format!("Failed to save the image: {e}"));
            return false;
        }
        self.file_path = Some(path);
        self.saved_depth = Some(self.undo_stack.len());
        self.mark_dirty();
        cx.notify();
        true
    }

    fn open_png(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("PNG image", &["png"])
            .pick_file()
        else {
            return; // canceled
        };
        self.load_image(&path, window, cx);
    }

    // Ask what to do with unsaved changes before the document is replaced.
    // Returns true when it is OK to continue (nothing to save, saved, or discarded).
    fn confirm_discard(&mut self, cx: &mut Context<Self>) -> bool {
        // flatten a floating selection first, so that it counts as a change
        self.commit_selection();
        self.drag = Drag::None;
        if !self.is_modified() {
            return true;
        }
        let answer = rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Warning)
            .set_title("min-paint")
            .set_description("The image has unsaved changes. Save them first?")
            .set_buttons(rfd::MessageButtons::YesNoCancel)
            .show();
        match answer {
            rfd::MessageDialogResult::Yes => self.save_png(false, cx), // a canceled save aborts too
            rfd::MessageDialogResult::No => true,
            _ => false,
        }
    }

    // start a new, empty document (asks about unsaved changes first)
    fn new_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.confirm_discard(cx) {
            return;
        }
        let bitmap = Bitmap::new(DEFAULT_CANVAS_SIZE.0, DEFAULT_CANVAS_SIZE.1, BACKGROUND);
        // a new document has no history and no file name
        self.replace_bitmap(bitmap, false);
        self.file_path = None;
        self.scroll = (0., 0.);
        self.after_history_change(window, cx);
    }

    // open an image file as a new document (used by the Open dialog and by file drops)
    fn load_image(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if !self.confirm_discard(cx) {
            return;
        }

        let img = match image::open(path) {
            Ok(img) => img.to_rgba8(),
            Err(e) => {
                show_error(&format!("Failed to open the image: {e}"));
                return;
            }
        };

        // opening a file starts a new document: the history is cleared
        let bitmap = Bitmap::from(img);
        self.replace_bitmap(bitmap, false);
        self.file_path = Some(path.to_path_buf());
        self.scroll = (0., 0.);
        self.after_history_change(window, cx);
    }

    // -- UI ------------------------------------------------------------------

    fn tool_button(&self, label: &'static str, icon: impl Into<Icon>, tool: Tool, cx: &mut Context<Self>) -> impl IntoElement {
        div().child(
            Toggle::new(label)
                .tooltip(label)
                .icon(icon)
                .small()
                .checked(self.tool == tool)
                .on_click(cx.listener(move |this, _checked, _, cx| {
                    // leaving the selection tool flattens the selection
                    if tool != Tool::Select {
                        this.commit_selection();
                    }
                    this.tool = tool;
                    this.mark_dirty();
                    cx.notify();
                })),
        )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {

        let new_button = Button::new("btn-new")
            .ghost().small().icon(IconName::File).tooltip("New (Ctrl+N)")
            .on_click(cx.listener(|this, _, window, cx| this.new_document(window, cx)));
        let open_button = Button::new("btn-open")
            .ghost().small().icon(IconName::FolderOpen).tooltip("Open (Ctrl+O)")
            .on_click(cx.listener(|this, _, window, cx| this.open_png(window, cx)));
        let save_button = Button::new("btn-save")
            .ghost().small().icon(IconName::Save).tooltip("Save (Ctrl+S)")
            .on_click(cx.listener(|this, _, _, cx| { this.save_png(false, cx); }));
        let save_as_button = Button::new("btn-save-as")
            .ghost().small().icon(IconName::SavePen).tooltip("Save As (Ctrl+Shift+S)")
            .on_click(cx.listener(|this, _, _, cx| { this.save_png(true, cx); }));
        let undo_button = Button::new("btn-undo")
            .ghost().small().icon(IconName::Undo2).tooltip("Undo (Ctrl+Z)")
            .on_click(cx.listener(|this, _, window, cx| this.undo(window, cx)));
        let redo_button = Button::new("btn-redo")
            .ghost().small().icon(IconName::Redo2).tooltip("Redo (Ctrl+Y)")
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
            .child(self.tool_button("Select", IconName::SquareDashedMousePointer, Tool::Select, cx))
            .child(Button::new("btn-resize")
                .small().icon(IconName::ImageUpscale).tooltip("Resize…").ghost()
                .on_click(cx.listener(|this, _, window, cx| this.open_resize_dialog(window, cx)))
            );

        // rectangle / ellipse style
        let filled_toggle = Toggle::new("filled").tooltip("Filled").small()
            .icon(IconName::Droplets).checked(self.shape_filled)
            .on_click(cx.listener(|this, checked, _, cx| {
                this.shape_filled = *checked; cx.notify();
            }));

        // anti-aliasing on/off
        let aa_toggle = Toggle::new("antialias")
            .tooltip("Anti-alias")
            .small().icon(IconName::ALargeSmall)
            .checked(self.antialias)
            .on_click(cx.listener(|this, checked, _, cx| {
                this.antialias = *checked;
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
            let color: Hsla = rgb(hex).into();
            let selected = self.current_color == color;
            grid = grid.child(
                div()
                    .w(px(SWATCH_SIZE))
                    .h(px(SWATCH_SIZE))
                    .bg(color)
                    .border_2()
                    .border_color(if selected { cx.theme().accent_foreground } else { cx.theme().border })
                    .cursor(CursorStyle::PointingHand)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            // change the drawing color for subsequent operations
                            this.current_color = color;
                            // keep the picker's displayed color in sync (does not emit Change)
                            this.color_picker
                                .update(cx, |state, cx| state.set_value(color, window, cx));
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
            .file_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "Untitled (not saved yet)".to_string());
        let base_name = self
            .file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_string());
        let file_text = if self.is_modified() { format!("*{base_name}") } else { base_name };

        let size_text = format!("{} × {} px", self.bitmap.width, self.bitmap.height);
        let cursor_text = self
            .cursor_px
            .map(|(x, y)| format!("{}, {}", x, y))
            .unwrap_or_default();

        // size of the selection (empty when nothing is selected)
        let selection_text = self
            .selection
            .as_ref()
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
            Drag::SelectMove { .. } => CursorStyle::ClosedHand,
            Drag::None if self.over_selection && self.selection.is_some() => CursorStyle::OpenHand,
            _ => CursorStyle::Crosshair,
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

/// convert a gpui color into an opaque RGBA pixel
fn to_rgba8(color: Hsla) -> [u8; 4] {
    let c: Rgba = color.into();
    let q = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    [q(c.r), q(c.g), q(c.b), 0xff]
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

fn show_error(message: &str) {
    // the release build has no console, so report errors with a dialog
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Error")
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

fn main() {

    application().with_assets(AllAssets).run(move |cx| {

        gpui_kit::init(cx);

        let window_options = window_options(cx);
        open_window(window_options, cx, |window, cx| cx.new(|cx| Application::new(window, cx)))
            .expect("failed to open window");

        cx.on_window_closed(|cx, _window_id| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        }).detach();

        cx.activate(true);

    });
}

fn window_options(cx: &mut App) -> WindowOptions {
    let bounds = Bounds::centered(None, size(px(900.), px(640.)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        focus: true,
        titlebar: Some(TitlebarOptions {
            title: Some(SharedString::new("min-paint")),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        ..Default::default()
    }
}
