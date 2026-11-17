use std::path::PathBuf;
use std::path::Path;

// -- geometry ---------------------------------------------------------

// arrow proportions, as multiples of the brush size
pub const ARROW_HEAD_LEN: f32 = 5.;
pub const ARROW_HEAD_HALF: f32 = 2.5; // half width of the head
pub const ARROW_TAIL_HALF: f32 = 1.;  // half width of the body where it is widest

// x0, y0, x1, y1 (end-exclusive)
pub type Rect = (i32, i32, i32, i32);
pub type Pt = (f32, f32);

pub fn union_rect(a: Rect, b: Rect) -> Rect {
    (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))
}

/// center of a canvas pixel in continuous coordinates
pub fn pixel_center(p: (i32, i32)) -> (f32, f32) {
    (p.0 as f32 + 0.5, p.1 as f32 + 0.5)
}

/// Signed distance from `p` to a triangle (negative inside).
/// Returns `f32::MAX` for a degenerate triangle, so that nothing is covered.
pub fn triangle_distance(tri: &[Pt; 3], p: Pt) -> f32 {
    let area = (tri[1].0 - tri[0].0) * (tri[2].1 - tri[0].1)
        - (tri[1].1 - tri[0].1) * (tri[2].0 - tri[0].0);
    if area.abs() < 1e-6 {
        return f32::MAX;
    }
    let sign = if area > 0. { 1. } else { -1. };
    let mut d = f32::MIN;
    for i in 0..3 {
        let (a, b) = (tri[i], tri[(i + 1) % 3]);
        let (ex, ey) = (b.0 - a.0, b.1 - a.1);
        let len = (ex * ex + ey * ey).sqrt();
        if len < 1e-6 {
            continue;
        }
        // outward unit normal of the edge
        let (nx, ny) = (ey * sign / len, -ex * sign / len);
        d = d.max((p.0 - a.0) * nx + (p.1 - a.1) * ny);
    }
    d
}

/// Bounding box of a triangle: (min_x, min_y, max_x, max_y).
pub fn triangle_bounds(tri: &[Pt; 3]) -> (f32, f32, f32, f32) {
    let xs = tri.map(|p| p.0);
    let ys = tri.map(|p| p.1);
    (
        xs.iter().cloned().fold(f32::MAX, f32::min),
        ys.iter().cloned().fold(f32::MAX, f32::min),
        xs.iter().cloned().fold(f32::MIN, f32::max),
        ys.iter().cloned().fold(f32::MIN, f32::max),
    )
}

/// An arrow from `start` to `end` as two triangles: a body that tapers to a point at the
/// start, and the head at the end. Returns `None` when the arrow has no length.
pub fn arrow_triangles(start: Pt, end: Pt, size: f32) -> Option<[[Pt; 3]; 2]> {
    let (dx, dy) = (end.0 - start.0, end.1 - start.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1. {
        return None;
    }
    let (ux, uy) = (dx / len, dy / len); // direction
    let (nx, ny) = (-uy, ux); // normal

    // shrink the whole arrow head and body when the drag is short, keeping the proportions
    let k = (len * 0.5 / (size * ARROW_HEAD_LEN)).min(1.);
    let head_len = size * ARROW_HEAD_LEN * k;
    let head_half = size * ARROW_HEAD_HALF * k;
    let tail_half = size * ARROW_TAIL_HALF * k;

    // base of the head
    let hb = (end.0 - ux * head_len, end.1 - uy * head_len);
    let head = [
        end,
        (hb.0 + nx * head_half, hb.1 + ny * head_half),
        (hb.0 - nx * head_half, hb.1 - ny * head_half),
    ];
    // the body ends inside the head, so that no seam shows between the two
    let tb = (end.0 - ux * head_len * 0.8, end.1 - uy * head_len * 0.8);
    let tail = [
        start,
        (tb.0 + nx * tail_half, tb.1 + ny * tail_half),
        (tb.0 - nx * tail_half, tb.1 - ny * tail_half),
    ];
    Some([tail, head])
}


// -- color ------------------------------------------------------------

/// straight RGBA -> premultiplied (all channels in 0..=255 as f32)
pub fn premul(p: [u8; 4]) -> [f32; 4] {
    let a = p[3] as f32 / 255.;
    [p[0] as f32 * a, p[1] as f32 * a, p[2] as f32 * a, p[3] as f32]
}

/// premultiplied -> straight RGBA; fully transparent pixels are always `[0, 0, 0, 0]`
pub fn unpremul(p: [f32; 4]) -> [u8; 4] {
    let a = p[3].clamp(0., 255.);
    if a < 0.5 {
        return [0, 0, 0, 0];
    }
    let k = 255. / a;
    let q = |v: f32| (v * k).round().clamp(0., 255.) as u8;
    [q(p[0]), q(p[1]), q(p[2]), a.round() as u8]
}

/// Interpolate from `from` to `to` by `c` (0..=255) in premultiplied space.
/// With an opaque `to` this is "source over"; with a transparent `to` it erases.
pub fn mix_px(from: [u8; 4], to: [u8; 4], c: u32) -> [u8; 4] {
    if from[3] == 255 && to[3] == 255 {
        let m = |f: u8, t: u8| ((f as u32 * (255 - c) + t as u32 * c + 127) / 255) as u8;
        return [m(from[0], to[0]), m(from[1], to[1]), m(from[2], to[2]), 0xff];
    }
    let (f, t) = (premul(from), premul(to));
    let k = c as f32 / 255.;
    unpremul(std::array::from_fn(|i| f[i] + (t[i] - f[i]) * k))
}

/// Porter-Duff "source over" for straight RGBA pixels.
pub fn over(src: [u8; 4], dst: [u8; 4]) -> [u8; 4] {
    match src[3] {
        255 => return src,
        0 => return dst,
        _ => {}
    }
    let (s, d) = (premul(src), premul(dst));
    let k = 1. - src[3] as f32 / 255.;
    unpremul(std::array::from_fn(|i| s[i] + d[i] * k))
}

// -- coverage ---------------------------------------------------------

/// Antialiasing coverage of the current stroke or shape (0..=255 per pixel).
pub struct Coverage {
    width: usize,
    height: usize,
    data: Vec<u8>,
    dirty: Option<Rect>,
}
impl Coverage {
    pub fn new(width: usize, height: usize) -> Self {
        Self { width, height, data: vec![0; width * height], dirty: None }
    }

    /// Resize if necessary, and clear only the modified region from the previous stroke.
    pub fn prepare(&mut self, width: usize, height: usize) {
        if self.width != width || self.height != height || self.data.len() != width * height {
            self.width = width;
            self.height = height;
            self.data.resize(width * height, 0);
            self.data.fill(0);
            self.dirty = None;
        } else if let Some(rect) = self.dirty.take() {
            self.clear_rect(rect);
        }
    }

    pub fn clear_all(&mut self) {
        if let Some(rect) = self.dirty.take() {
            self.clear_rect(rect);
        }
    }

    fn mark_dirty(&mut self, rect: Rect) {
        if rect.0 >= rect.2 || rect.1 >= rect.3 {
            return;
        }
        self.dirty = Some(match self.dirty {
            Some(d) => union_rect(d, rect),
            None => rect,
        });
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

    pub fn clear_rect(&mut self, rect: Rect) {
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
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let v = (value.clamp(0., 1.) * 255.).round() as u8;
        let i = y as usize * self.width + x as usize;
        if v > self.data[i] {
            self.data[i] = v;
        }
    }

    /// A line segment with round caps; the line width is `2 * radius`.
    pub fn add_capsule(&mut self, a: (f32, f32), b: (f32, f32), radius: f32) -> Rect {
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
        self.mark_dirty(rect);
        rect
    }

    /// Ellipse outline, drawn as a chain of short capsules.
    pub fn add_ellipse_outline(&mut self, cx: f32, cy: f32, a: f32, b: f32, radius: f32) -> Rect {
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
        let rect = rect.unwrap_or((0, 0, 0, 0));
        self.mark_dirty(rect);
        rect
    }

    /// Filled ellipse; the edge coverage comes from an approximate distance to the boundary.
    pub fn add_ellipse_fill(&mut self, cx: f32, cy: f32, a: f32, b: f32) -> Rect {
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
        self.mark_dirty(rect);
        rect
    }

    /// Filled triangle with an antialiased edge.
    pub fn add_triangle(&mut self, tri: &[Pt; 3]) -> Rect {
        let (min_x, min_y, max_x, max_y) = triangle_bounds(tri);
        let rect = self.clip(min_x - 1., min_y - 1., max_x + 1., max_y + 1.);
        for y in rect.1..rect.3 {
            for x in rect.0..rect.2 {
                let d = triangle_distance(tri, (x as f32 + 0.5, y as f32 + 0.5));
                self.put(x, y, 0.5 - d);
            }
        }
        self.mark_dirty(rect);
        rect
    }
}

// -- pixmap -----------------------------------------------------------

// transparent canvas background
pub const BACKGROUND: [u8; 4] = [0, 0, 0, 0];
pub const BACKGROUND_WHITE: [u8; 4] = [0xff, 0xff, 0xff, 0xff];

/// An opaque RGBA pixel map: the source of truth of the document.
/// It tracks the bounding box of modified pixels so that history entries stay small.
#[derive(Clone)]
pub struct Pixmap {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<[u8; 4]>,
    // modified area since the last history entry: (x0, y0, x1, y1), end-exclusive
    pub dirty: Option<Rect>,
}

impl Pixmap {
    pub fn new(width: usize, height: usize, fill: [u8; 4]) -> Self {
        Self { width, height, pixels: vec![fill; width * height], dirty: None }
    }
    pub fn from_image(img: image::RgbaImage) -> Self {
        let (width, height) = (img.width() as usize, img.height() as usize);
        let pixels = img.pixels()
            .map(|p| if p[3] == 0 { [0, 0, 0, 0] } else { [p[0], p[1], p[2], p[3]] })
            .collect();
        Self { width, height, pixels, dirty: None }
    }
    // grow the dirty area; the given rectangle must already be clipped to the pixel map
    fn mark(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        self.dirty = Some(match self.dirty {
            Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
            None => (x0, y0, x1, y1),
        });
    }

    pub fn get(&self, x: i32, y: i32) -> Option<[u8; 4]> {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return None;
        }
        Some(self.pixels[y as usize * self.width + x as usize])
    }

    pub fn set(&mut self, x: i32, y: i32, color: [u8; 4]) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        self.pixels[y as usize * self.width + x as usize] = color;
        self.mark(x, y, x + 1, y + 1);
    }

    /// Fill a rectangle, clipped to the pixel map.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: [u8; 4]) {
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
    pub fn stamp(&mut self, cx: i32, cy: i32, size: i32, color: [u8; 4]) {
        self.fill_rect(cx - size / 2, cy - size / 2, size, size, color);
    }

    /// Draw a line with Bresenham's algorithm, stamping the brush at every step.
    pub fn line(&mut self, from: (i32, i32), to: (i32, i32), size: i32, color: [u8; 4]) {
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
    pub fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, size: i32, filled: bool, color: [u8; 4]) {
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
    pub fn ellipse(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, size: i32, filled: bool, color: [u8; 4]) {
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

    /// Fill a triangle (pixels whose center is inside), clipped to the pixel map.
    fn fill_triangle(&mut self, tri: &[Pt; 3], color: [u8; 4]) {
        let (min_x, min_y, max_x, max_y) = triangle_bounds(tri);
        let x0 = (min_x.floor() as i32).max(0);
        let y0 = (min_y.floor() as i32).max(0);
        let x1 = (max_x.ceil() as i32).min(self.width as i32);
        let y1 = (max_y.ceil() as i32).min(self.height as i32);
        for y in y0..y1 {
            for x in x0..x1 {
                if triangle_distance(tri, (x as f32 + 0.5, y as f32 + 0.5)) <= 0. {
                    self.set(x, y, color);
                }
            }
        }
    }

    /// Arrow from `start` to `end` (see `arrow_triangles`), without antialiasing.
    pub fn arrow(&mut self, start: (i32, i32), end: (i32, i32), size: i32, color: [u8; 4]) {
        let Some(tris) = arrow_triangles(pixel_center(start), pixel_center(end), size as f32) else {
            return;
        };
        for tri in &tris {
            self.fill_triangle(tri, color);
        }
    }

    /// Scanline flood fill (4-connected) that replaces the region of the clicked color.
    /// With `smooth_edges`, the antialiased pixels around the region are recolored too.
    pub fn flood_fill(&mut self, x: i32, y: i32, color: [u8; 4], smooth_edges: bool) {
        let Some(target) = self.get(x, y) else { return };
        if target == color {
            return;
        }

        let w = self.width;
        let height = self.height as i32;
        // 0 = unvisited, 1 = filled region, 2 = edge visited
        let mut visited = vec![0u8; w * self.height];
        let mut spans: Vec<(i32, i32, i32)> = Vec::new(); // (y, left, right), inclusive

        let mut stack = vec![(x, y)];
        while let Some((x, y)) = stack.pop() {
            if self.get(x, y) != Some(target) || visited[y as usize * w + x as usize] != 0 {
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
            let row_offset = y as usize * w;
            for i in left..=right {
                visited[row_offset + i as usize] = 1;
            }
            spans.push((y, left, right));

            // queue one seed per run of target pixels in the rows above and below
            for ny in [y - 1, y + 1] {
                if ny < 0 || ny >= height {
                    continue;
                }
                let mut inside = false;
                let nrow_offset = ny as usize * w;
                for i in left..=right {
                    let matches = self.get(i, ny) == Some(target) && visited[nrow_offset + i as usize] == 0;
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
            for &(y, left, right) in &spans {
                // left and right neighbors on the same row
                for nx in [left - 1, right + 1] {
                    if nx >= 0 && nx < w as i32 {
                        let i = y as usize * w + nx as usize;
                        if visited[i] == 0 {
                            visited[i] = 2;
                            if let Some(c) = self.blend_edge(nx, y, target, color) {
                                edges.push((nx, y, c));
                            }
                        }
                    }
                }

                // rows above and below: scan from left - 1 to right + 1
                for ny in [y - 1, y + 1] {
                    if ny < 0 || ny >= height {
                        continue;
                    }
                    let nrow_offset = ny as usize * w;
                    let x0 = (left - 1).max(0) as usize;
                    let x1 = ((right + 1).min(w as i32 - 1) + 1) as usize;
                    for nx in x0..x1 {
                        if visited[nrow_offset + nx] == 0 {
                            visited[nrow_offset + nx] = 2;
                            if let Some(c) = self.blend_edge(nx as i32, ny, target, color) {
                                edges.push((nx as i32, ny, c));
                            }
                        }
                    }
                }
            }
        }

        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;

        for &(y, left, right) in &spans {
            let x0 = left.max(0);
            let x1 = (right + 1).min(w as i32);
            if x0 < x1 {
                let row = y as usize * w;
                self.pixels[row + x0 as usize..row + x1 as usize].fill(color);
                min_x = min_x.min(x0);
                max_x = max_x.max(x1);
                min_y = min_y.min(y);
                max_y = max_y.max(y + 1);
            }
        }
        for (x, y, c) in edges {
            let i = y as usize * w + x as usize;
            self.pixels[i] = c;
            min_x = min_x.min(x);
            max_x = max_x.max(x + 1);
            min_y = min_y.min(y);
            max_y = max_y.max(y + 1);
        }
        if min_x < max_x && min_y < max_y {
            self.mark(min_x, min_y, max_x, max_y);
        }
    }

    fn blend_edge(&self, x: i32, y: i32, target: [u8; 4], color: [u8; 4]) -> Option<[u8; 4]> {
        let c = self.get(x, y)?;
        if c == target {
            return None; // not connected to the region, so leave it alone
        }
        // work in premultiplied space so that antialiased edges against a
        // transparent background are handled correctly
        let (t, col, pc) = (premul(target), premul(color), premul(c));
        let dist2 = |p: [f32; 4]| -> f32 { (0..4).map(|k| (p[k] - t[k]).powi(2)).sum() };
        // the most distant color nearby stands in for the line color
        let mut line = pc;
        for dy in -1..=1 {
            for dx in -1..=1 {
                if let Some(n) = self.get(x + dx, y + dy) {
                    let n = premul(n);
                    if dist2(n) > dist2(line) {
                        line = n;
                    }
                }
            }
        }
        let d: [f32; 4] = std::array::from_fn(|k| line[k] - t[k]);
        let len2: f32 = d.iter().map(|v| v * v).sum();
        if len2 < 1e-6 {
            return None;
        }
        // share of the line color in this pixel (1.0 = solid line)
        let a = ((0..4).map(|k| (pc[k] - t[k]) * d[k]).sum::<f32>() / len2).clamp(0., 1.);
        Some(unpremul(std::array::from_fn(|k| pc[k] + (1. - a) * (col[k] - t[k]))))
    }

    /// Pixelate a rectangle: every `block`-sized cell is replaced by its average color.
    /// Cells are anchored at the top-left of the rectangle, and the averaging is done in
    /// premultiplied space so that transparent pixels do not darken the result.
    pub fn mosaic(&mut self, x: i32, y: i32, w: i32, h: i32, block: i32) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.width as i32);
        let y1 = (y + h).min(self.height as i32);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let block = block.max(1);

        let mut by = y0;
        while by < y1 {
            let by_end = (by + block).min(y1);
            let mut bx = x0;
            while bx < x1 {
                let bx_end = (bx + block).min(x1);

                // average of the cell
                let mut sum = [0f32; 4];
                for yy in by..by_end {
                    let row = yy as usize * self.width;
                    for xx in bx..bx_end {
                        let p = premul(self.pixels[row + xx as usize]);
                        for k in 0..4 {
                            sum[k] += p[k];
                        }
                    }
                }
                let n = ((bx_end - bx) * (by_end - by)) as f32;
                let avg = unpremul(sum.map(|v| v / n));

                // fill the cell
                for yy in by..by_end {
                    let row = yy as usize * self.width;
                    self.pixels[row + bx as usize..row + bx_end as usize].fill(avg);
                }
                bx = bx_end;
            }
            by = by_end;
        }
        self.mark(x0, y0, x1, y1);
    }

    /// Copy a rectangle into a new pixel map (pixels outside of this pixel map become background).
    pub fn copy_region(&self, x: i32, y: i32, w: i32, h: i32) -> Pixmap {
        let mut out = Pixmap::new(w.max(1) as usize, h.max(1) as usize, BACKGROUND);
        for yy in 0..h {
            for xx in 0..w {
                if let Some(p) = self.get(x + xx, y + yy) {
                    out.pixels[yy as usize * out.width + xx as usize] = p;
                }
            }
        }
        out
    }

    /// Overwrite a part of this pixel map with `src` placed at (x, y), clipped.
    pub fn paste(&mut self, src: &Pixmap, x: i32, y: i32) {
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
                let i = dy as usize * self.width + dx as usize;
                self.pixels[i] = over(src.pixels[sy as usize * src.width + sx as usize], self.pixels[i]);
            }
        }
        self.mark(
            x.max(0),
            y.max(0),
            (x + src.width as i32).min(self.width as i32),
            (y + src.height as i32).min(self.height as i32),
        );
    }

    /// Restore a rectangle from `src` (a pixel map of the same size), clipped.
    pub fn restore_region(&mut self, src: &Pixmap, rect: Rect) {
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
    pub fn composite(&mut self, backup: &Pixmap, cov: &Coverage, color: [u8; 4], rect: Rect) {
        let x0 = rect.0.max(0);
        let y0 = rect.1.max(0);
        let x1 = rect.2.min(self.width as i32);
        let y1 = rect.3.min(self.height as i32);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for y in y0..y1 {
            let row = y as usize * self.width;
            for x in x0..x1 {
                let i = row + x as usize;
                let c = cov.data[i] as u32;
                let b = backup.pixels[i];
                self.pixels[i] = match c {
                    0 => b,
                    255 => color,
                    _ => mix_px(b, color, c),
                };
            }
        }
        self.mark(x0, y0, x1, y1);
    }

    // history helpers: these do not touch the dirty area
    pub fn read_region(&self, x: usize, y: usize, w: usize, h: usize) -> Vec<[u8; 4]> {
        let mut out = Vec::with_capacity(w * h);
        for yy in y..y + h {
            let row = yy * self.width;
            out.extend_from_slice(&self.pixels[row + x..row + x + w]);
        }
        out
    }

    pub fn write_region(&mut self, x: usize, y: usize, w: usize, h: usize, data: &[[u8; 4]]) {
        for (i, yy) in (y..y + h).enumerate() {
            let row = yy * self.width;
            self.pixels[row + x..row + x + w].copy_from_slice(&data[i * w..(i + 1) * w]);
        }
    }

}

// -- canvas size --------------------------------------------------------------

pub const MAX_CANVAS_SIZE: usize = 4096;

/// The requested canvas size is zero or larger than `MAX_CANVAS_SIZE`.
#[derive(Debug)]
pub struct SizeError;

impl std::fmt::Display for SizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "canvas size must be between 1 and {MAX_CANVAS_SIZE} pixels")
    }
}

pub fn is_valid_canvas_size(width: usize, height: usize) -> bool {
    width > 0 && height > 0 && width <= MAX_CANVAS_SIZE && height <= MAX_CANVAS_SIZE
}

// -- history & document -----------------------------------------------------

/// Maximum number of undo steps kept.
pub const MAX_HISTORY: usize = 100;

/// One undo step. A patch stores only the changed rectangle.
pub enum Edit {
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
        before: Pixmap,
        after: Pixmap,
    },
}


/// A rectangular selection. The pixels are lifted ("floating") when the selection is first moved.
pub struct Selection {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub floating: Option<Pixmap>,
}
impl Selection {
    pub fn contains(&self, p: (i32, i32)) -> bool {
        p.0 >= self.x && p.0 < self.x + self.w && p.1 >= self.y && p.1 < self.y + self.h
    }
}

/// The document: pixels, undo / redo history, selection and file name.
/// It knows nothing about the UI, so both the GUI and MCP can drive it.
pub struct Document {
    pub pixmap: Pixmap,
    // the pixel map as of the last history entry, used to build patches
    committed: Pixmap,
    undo_stack: Vec<Edit>,
    redo_stack: Vec<Edit>,
    // undo depth of the saved (or freshly opened) document state;
    // None when that state can no longer be reached with undo / redo
    saved_depth: Option<usize>,
    pub selection: Option<Selection>,
    // file the document was opened from or last saved to (None = Untitled)
    pub file_path: Option<PathBuf>,
}
impl Document {
    pub fn new(width: usize, height: usize, fill: [u8; 4]) -> Self {
        let pixmap = Pixmap::new(width, height, fill);
        Self {
            committed: pixmap.clone(),
            pixmap,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            saved_depth: Some(0),
            selection: None,
            file_path: None,
        }
    }
    // -- history --------------------------------------------------------------

    /// true when the document differs from the saved state
    pub fn is_modified(&self) -> bool {
        // pixels that were lifted by a selection move are not in the history yet
        let lifted = self.selection.as_ref().is_some_and(|s| s.floating.is_some());
        lifted || self.saved_depth != Some(self.undo_stack.len())
    }

    /// Current undo depth; pass it to `mark_saved` once the file has been written.
    pub fn undo_depth(&self) -> usize {
        self.undo_stack.len()
    }

    pub fn mark_saved(&mut self, depth: usize) {
        self.saved_depth = Some(depth);
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

    /// Turn the pixels modified since the last entry into one undo step.
    pub fn record_history(&mut self) {
        let Some((x0, y0, x1, y1)) = self.pixmap.dirty.take() else { return };
        let (x, y, w, h) = (x0 as usize, y0 as usize, (x1 - x0) as usize, (y1 - y0) as usize);
        let before = self.committed.read_region(x, y, w, h);
        let after = self.pixmap.read_region(x, y, w, h);
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
                self.pixmap.write_region(*x, *y, *w, *h, data);
                self.committed.write_region(*x, *y, *w, *h, data);
            }
            Edit::Replace { before, after } => {
                let target = if undo { before } else { after };
                self.pixmap = target.clone();
                self.committed = target.clone();
            }
        }
        self.pixmap.dirty = None;
    }

    /// Returns true when an edit was undone.
    pub fn undo(&mut self) -> bool {
        // flatten a floating selection first, so that undo cancels the move
        self.commit_selection();
        let Some(edit) = self.undo_stack.pop() else { return false };
        self.apply_edit(&edit, true);
        self.redo_stack.push(edit);
        true
    }

    /// Returns true when an edit was redone.
    pub fn redo(&mut self) -> bool {
        self.commit_selection();
        let Some(edit) = self.redo_stack.pop() else { return false };
        self.apply_edit(&edit, false);
        self.undo_stack.push(edit);
        true
    }

    /// Replace the whole pixel map (resize / open); `record` makes it undoable.
    pub fn replace_pixmap(&mut self, new_pixmap: Pixmap, record: bool) {
        self.selection = None;
        if record {
            self.record_history();
            let before = std::mem::replace(&mut self.committed, new_pixmap.clone());
            self.push_edit(Edit::Replace { before, after: new_pixmap.clone() });
        } else {
            self.committed = new_pixmap.clone();
            self.undo_stack.clear();
            self.redo_stack.clear();
            self.saved_depth = Some(0);
        }
        self.pixmap = new_pixmap;
    }

    // -- selection ------------------------------------------------------------

    /// Flatten the selection into the pixel map (a no-op if the pixels were never lifted).
    pub fn commit_selection(&mut self) {
        let Some(sel) = self.selection.take() else { return };
        if let Some(floating) = sel.floating {
            self.pixmap.paste(&floating, sel.x, sel.y);
            self.record_history();
        }
    }

    /// Select the whole canvas (the previous selection is flattened first).
    pub fn select_all(&mut self) {
        self.commit_selection();
        self.selection = Some(Selection {
            x: 0,
            y: 0,
            w: self.pixmap.width as i32,
            h: self.pixmap.height as i32,
            floating: None,
        });
    }

    pub fn delete_selection(&mut self) {
        let Some(sel) = self.selection.take() else { return };
        // lifted pixels already left a hole behind, so only an unlifted selection needs clearing
        if sel.floating.is_none() {
            self.pixmap.fill_rect(sel.x, sel.y, sel.w, sel.h, BACKGROUND);
        }
        self.record_history();
    }

    /// Select the rectangle between two inclusive corners, clamped to the canvas.
    pub fn select_rect(&mut self, start: (i32, i32), end: (i32, i32)) {
        let max_x = self.pixmap.width as i32 - 1;
        let max_y = self.pixmap.height as i32 - 1;
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

    /// Start moving the selection when `p` is inside it: the pixels are lifted
    /// (leaving a background-colored hole) and the grab offset is returned.
    pub fn grab_selection(&mut self, p: (i32, i32)) -> Option<(i32, i32)> {
        let sel = self.selection.as_mut().filter(|s| s.contains(p))?;
        if sel.floating.is_none() {
            sel.floating = Some(self.pixmap.copy_region(sel.x, sel.y, sel.w, sel.h));
            self.pixmap.fill_rect(sel.x, sel.y, sel.w, sel.h, BACKGROUND);
        }
        Some((p.0 - sel.x, p.1 - sel.y))
    }

    /// The pixels of the selection (lifted ones, or a copy from the pixel map).
    pub fn selection_pixmap(&self) -> Option<Pixmap> {
        let sel = self.selection.as_ref()?;
        Some(match &sel.floating {
            Some(floating) => floating.clone(),
            None => self.pixmap.copy_region(sel.x, sel.y, sel.w, sel.h),
        })
    }

    /// Trim the canvas to `width` x `height`, starting at the top-left corner of the
    /// selection (the canvas origin if nothing is selected). The part outside the current
    /// canvas becomes the background.
    /// Returns `Ok(false)` when nothing would change, and `Err` for an invalid size
    /// (checked before the selection is touched).
    pub fn trim(&mut self, width: usize, height: usize) -> Result<bool, SizeError> {
        if !is_valid_canvas_size(width, height) {
            return Err(SizeError);
        }
        // read the origin before the selection is flattened
        let (ox, oy) = self.selection.as_ref().map_or((0, 0), |s| (s.x, s.y));
        if (ox, oy) == (0, 0) && width == self.pixmap.width && height == self.pixmap.height {
            return Ok(false);
        }
        // flatten a floating selection so that its pixels are part of the trimmed image
        self.commit_selection();
        let new_pixmap = self.pixmap.copy_region(ox, oy, width as i32, height as i32);
        self.replace_pixmap(new_pixmap, true);
        Ok(true)
    }
    /// Grow the canvas to at least `width` x `height`, keeping the existing pixels in the
    /// top-left corner (undoable). Returns `Ok(false)` when it already is that large.
    pub fn grow_to(&mut self, width: usize, height: usize) -> Result<bool, SizeError> {
        let (old_w, old_h) = (self.pixmap.width, self.pixmap.height);
        let (new_w, new_h) = (width.max(old_w), height.max(old_h));
        if (new_w, new_h) == (old_w, old_h) {
            return Ok(false);
        }
        if !is_valid_canvas_size(new_w, new_h) {
            return Err(SizeError);
        }
        self.commit_selection();
        let mut grown = Pixmap::new(new_w, new_h, BACKGROUND);
        let data = self.pixmap.read_region(0, 0, old_w, old_h);
        grown.write_region(0, 0, old_w, old_h, &data);
        self.replace_pixmap(grown, true);
        Ok(true)
    }

    /// Place `pixmap` as a floating selection at (x, y). The previous selection is
    /// flattened first; the pixels are committed when the selection is.
    pub fn paste_floating(&mut self, pixmap: Pixmap, x: i32, y: i32) {
        self.commit_selection();
        let (w, h) = (pixmap.width as i32, pixmap.height as i32);
        self.selection = Some(Selection { x, y, w, h, floating: Some(pixmap) });
    }
}

// -- tools & painter ---------------------------------------------------------

/// Mosaic cell size = brush size * this factor (so the brush size controls the coarseness).
pub const MOSAIC_BLOCK_FACTOR: i32 = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Pencil,
    Eraser,
    Fill,
    Picker,
    Line,
    Rectangle,
    Oval,
    Text,
    Arrow,
    Mosaic,
    Select,
}

/// Tool settings and the state of the stroke / shape in progress.
/// It never owns the pixels: every method receives the target `Pixmap`.
pub struct Painter {
    pub tool: Tool,
    pub color: [u8; 4],
    pub brush_size: i32,
    pub shape_filled: bool,
    // anti-aliased drawing on/off
    pub antialias: bool,
    // coverage of the stroke or shape in progress (AA only)
    coverage: Coverage,
    // the pixel map before the stroke / shape; the AA result is composited over it
    backup: Option<Pixmap>,
    // the area the shape preview covers
    shape_bounds: Option<Rect>,
    // whether the stroke or shape in progress is anti-aliased
    active_aa: bool,
}

impl Painter {
    pub fn new(color: [u8; 4]) -> Self {
        Self {
            tool: Tool::Pencil,
            color,
            brush_size: 3,
            shape_filled: false,
            antialias: true,
            // resized by `prepare` when a stroke starts
            coverage: Coverage::new(0, 0),
            backup: None,
            shape_bounds: None,
            active_aa: false,
        }
    }

    /// Color used by the pencil-like tools.
    pub fn tool_color(&self) -> [u8; 4] {
        match self.tool {
            Tool::Eraser => BACKGROUND,
            _ => self.color,
        }
    }

    /// End the stroke or shape in progress. The caller records the history afterwards.
    pub fn finish(&mut self) {
        self.backup = None;
        self.shape_bounds = None;
        self.coverage.clear_all();
        self.active_aa = false;
    }

    // -- freehand (pencil / eraser) ---------------------------------------------

    pub fn begin_stroke(&mut self, pixmap: &mut Pixmap, p: (i32, i32)) {
        let color = self.tool_color();
        if self.antialias {
            // keep the pixels from before the stroke; the stroke is composited over them
            self.backup = Some(pixmap.clone());
            self.coverage.prepare(pixmap.width, pixmap.height);
            self.active_aa = true;
            self.aa_segment(pixmap, p, p, color);
        } else {
            self.active_aa = false;
            pixmap.stamp(p.0, p.1, self.brush_size, color);
        }
    }

    pub fn stroke_to(&mut self, pixmap: &mut Pixmap, from: (i32, i32), to: (i32, i32)) {
        let color = self.tool_color();
        if self.active_aa {
            self.aa_segment(pixmap, from, to, color);
        } else {
            pixmap.line(from, to, self.brush_size, color);
        }
    }

    // add a round-capped segment to the stroke's coverage and composite it over the saved pixels
    fn aa_segment(&mut self, pixmap: &mut Pixmap, from: (i32, i32), to: (i32, i32), color: [u8; 4]) {
        let Some(backup) = self.backup.as_ref() else { return };
        let radius = self.brush_size as f32 / 2.;
        let rect = self.coverage.add_capsule(pixel_center(from), pixel_center(to), radius);
        pixmap.composite(backup, &self.coverage, color, rect);
    }

    // -- shapes (line / rectangle / oval / arrow) -------------------------------

    pub fn begin_shape(&mut self, pixmap: &mut Pixmap, p: (i32, i32)) {
        self.backup = Some(pixmap.clone());
        self.active_aa = self.antialias;
        if self.active_aa {
            self.coverage.prepare(pixmap.width, pixmap.height);
        }
        self.shape_bounds = None;
        self.update_shape(pixmap, p, p);
    }

    /// Draw (or redraw) the shape preview between two canvas pixels.
    pub fn update_shape(&mut self, pixmap: &mut Pixmap, start: (i32, i32), end: (i32, i32)) {
        let Some(backup) = self.backup.as_ref() else { return };
        // erase the previous preview
        if let Some(bounds) = self.shape_bounds {
            pixmap.restore_region(backup, bounds);
            if self.active_aa {
                self.coverage.clear_rect(bounds);
            }
        }

        let color = self.tool_color();
        let size = self.brush_size;
        let (x0, x1) = (start.0.min(end.0), start.0.max(end.0));
        let (y0, y1) = (start.1.min(end.1), start.1.max(end.1));
        if self.active_aa {
            let cov = &mut self.coverage;
            // anti-aliased: build the coverage, then composite it over the original pixels
            let r = size as f32 / 2.;
            let touched: Option<Rect> = match self.tool {
                Tool::Line => Some(cov.add_capsule(pixel_center(start), pixel_center(end), r)),
                // a filled rectangle is pixel-aligned, so it needs no smoothing
                Tool::Rectangle if self.shape_filled => {
                    pixmap.rect(x0, y0, x1, y1, size, true, color);
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
                Tool::Arrow => {
                    arrow_triangles(pixel_center(start), pixel_center(end), size as f32).map(|[tail, head]| {
                        let r1 = cov.add_triangle(&tail);
                        let r2 = cov.add_triangle(&head);
                        union_rect(r1, r2)
                    })
                }
                _ => None,
            };
            if let Some(rect) = touched {
                pixmap.composite(backup, cov, color, rect);
            }
        } else {
            match self.tool {
                Tool::Line => pixmap.line(start, end, size, color),
                Tool::Rectangle => pixmap.rect(x0, y0, x1, y1, size, self.shape_filled, color),
                Tool::Oval => pixmap.ellipse(x0, y0, x1, y1, size, self.shape_filled, color),
                Tool::Arrow => pixmap.arrow(start, end, size, color),
                _ => {}
            }
        }
        // the preview may extend beyond the corners by the brush size
        // (an arrow head is wider than the brush)
        let margin = if self.tool == Tool::Arrow { size * 3 + 2 } else { size };
        self.shape_bounds = Some((x0 - margin, y0 - margin, x1 + margin + 1, y1 + margin + 1));
    }

    // -- other tools ------------------------------------------------------------

    pub fn fill(&self, pixmap: &mut Pixmap, p: (i32, i32)) {
        pixmap.flood_fill(p.0, p.1, self.color, self.antialias);
    }

    pub fn mosaic(&self, pixmap: &mut Pixmap, x: i32, y: i32, w: i32, h: i32) {
        pixmap.mosaic(x, y, w, h, self.brush_size * MOSAIC_BLOCK_FACTOR);
    }

    /// Draw a whole shape in one call, as one undo step (no mouse drag involved).
    /// The painter's tool is only changed for the duration of the call.
    pub fn draw_shape_once(&mut self, doc: &mut Document, tool: Tool, start: (i32, i32), end: (i32, i32)) {
        doc.commit_selection();
        let previous = std::mem::replace(&mut self.tool, tool);
        self.begin_shape(&mut doc.pixmap, start);
        self.update_shape(&mut doc.pixmap, start, end);
        self.finish();
        self.tool = previous;
        doc.record_history();
    }
}

// -- text ---------------------------------------------------------------------

/// Font management and text drawing (cosmic-text). No GUI dependency.
#[derive(Default)]
pub struct TextRenderer {
    font_system: Option<cosmic_text::FontSystem>,
    swash_cache: Option<cosmic_text::SwashCache>,
    // installed font families (sorted, unique); built on first use
    families: Vec<String>,
    // font chosen last time in the text dialog
    pub last_font: Option<String>,
}

impl TextRenderer {
    /// Installed font families (collected once, on first use).
    pub fn families(&mut self) -> &[String] {
        if self.families.is_empty() {
            let font_system = self.font_system.get_or_insert_with(cosmic_text::FontSystem::new);
            let mut families: Vec<String> = font_system
                .db()
                .faces()
                .filter_map(|face| face.families.first().map(|(name, _)| name.clone()))
                // names starting with '.' are hidden system fonts (e.g. ".AI Bayan PUA" on macOS)
                .filter(|name| !name.starts_with('.'))
                .collect();
            families.sort_by_key(|s| s.to_lowercase());
            families.dedup();
            if families.is_empty() {
                families.push("sans-serif".to_string());
            }
            self.families = families;
        }
        &self.families
    }

    /// Default font for the first use: the first installed font of a per-OS candidate list.
    /// Call `families()` first, so that the list is loaded.
    pub fn default_family(&self) -> Option<String> {
        const CANDIDATES: &[&str] = if cfg!(target_os = "macos") {
            &["Hiragino Sans", "Helvetica Neue", "Helvetica", "Arial"]
        } else if cfg!(windows) {
            &["Yu Gothic UI", "Meiryo UI", "Segoe UI", "Arial"]
        } else {
            &["Noto Sans CJK JP", "Noto Sans", "DejaVu Sans", "Arial"]
        };
        CANDIDATES
            .iter()
            .find_map(|c| self.families.iter().find(|f| f.as_str() == *c))
            .cloned()
    }

    /// Draw `text` with its top-left corner at `origin`.
    /// Returns true when at least one pixel was drawn; the caller records the history.
    pub fn render(
        &mut self,
        pixmap: &mut Pixmap,
        text: &str,
        font_size: f32,
        family: Option<&str>,
        origin: (i32, i32),
        color: [u8; 4],
        antialias: bool,
    ) -> bool {
        let font_system = self.font_system.get_or_insert_with(cosmic_text::FontSystem::new);
        let swash_cache = self.swash_cache.get_or_insert_with(cosmic_text::SwashCache::new);

        let metrics = cosmic_text::Metrics::new(font_size, font_size * 1.25);
        let mut buffer = cosmic_text::Buffer::new(font_system, metrics);
        let mut buffer = buffer.borrow_with(font_system);
        let attrs = match family {
            Some(name) => cosmic_text::Attrs::new().family(cosmic_text::Family::Name(name)),
            None => cosmic_text::Attrs::new(),
        };
        buffer.set_text(text, &attrs, cosmic_text::Shaping::Advanced, None);
        buffer.shape_until_scroll(false);

        let cosmic_color = cosmic_text::Color::rgba(color[0], color[1], color[2], color[3]);
        let bw = pixmap.width as i32;
        let bh = pixmap.height as i32;

        let mut drawn = false;
        buffer.draw(swash_cache, cosmic_color, |x, y, _w, _h, pixel_color| {
            let px = origin.0 + x;
            let py = origin.1 + y;
            if px < 0 || py < 0 || px >= bw || py >= bh {
                return;
            }
            let a = pixel_color.a();
            if a == 0 {
                return;
            }
            let src = [pixel_color.r(), pixel_color.g(), pixel_color.b(), a];
            if !antialias {
                if a >= 128 {
                    pixmap.set(px, py, [src[0], src[1], src[2], color[3]]);
                    drawn = true;
                }
            } else if let Some(bg) = pixmap.get(px, py) {
                pixmap.set(px, py, over(src, bg));
                drawn = true;
            }
        });
        drawn
    }
}

// -- image I/O ----------------------------------------------------------------

/// Errors of image encoding / decoding / file access. The UI localizes them.
#[derive(Debug)]
pub enum ImageError {
    UnsupportedExtension,
    Build,
    Encode(String),
    Save(String),
    Open(String),
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedExtension => write!(f, "unsupported file extension"),
            Self::Build => write!(f, "failed to build the image"),
            Self::Encode(e) => write!(f, "failed to encode the image: {e}"),
            Self::Save(e) => write!(f, "failed to save the image: {e}"),
            Self::Open(e) => write!(f, "failed to open the image: {e}"),
        }
    }
}

/// The image format of a path, if it can be written with the `image` crate.
pub fn save_format(path: &Path) -> Option<image::ImageFormat> {
    image::ImageFormat::from_path(path).ok().filter(|f| f.can_write())
}

impl Pixmap {
    pub fn to_rgba_image(&self) -> Option<image::RgbaImage> {
        let data: Vec<u8> = self.pixels.iter().flatten().copied().collect();
        image::RgbaImage::from_raw(self.width as u32, self.height as u32, data)
    }

    /// Encode as PNG (clipboard, MCP image content).
    pub fn to_png(&self) -> Result<Vec<u8>, ImageError> {
        let img = self.to_rgba_image().ok_or(ImageError::Build)?;
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|e| ImageError::Encode(e.to_string()))?;
        Ok(png)
    }

    /// Load an image file.
    pub fn open(path: &Path) -> Result<Pixmap, ImageError> {
        let img = image::open(path).map_err(|e| ImageError::Open(e.to_string()))?;
        Ok(Pixmap::from_image(img.to_rgba8()))
    }

    /// Write to a file; the format comes from the extension.
    pub fn save(&self, path: &Path) -> Result<(), ImageError> {
        let format = save_format(path).ok_or(ImageError::UnsupportedExtension)?;

        // formats that can store transparency; the others get the image flattened onto white
        let keep_alpha = matches!(
            format,
            image::ImageFormat::Png | image::ImageFormat::WebP | image::ImageFormat::Gif | image::ImageFormat::Tiff
        );
        let (w, h) = (self.width as u32, self.height as u32);
        let dynamic = if keep_alpha {
            self.to_rgba_image().map(image::DynamicImage::ImageRgba8)
        } else {
            let data: Vec<u8> = self.pixels.iter().flat_map(|p| {
                let a = p[3] as u32;
                let f = |c: u8| ((c as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
                [f(p[0]), f(p[1]), f(p[2])]
            }).collect();
            image::RgbImage::from_raw(w, h, data).map(image::DynamicImage::ImageRgb8)
        };
        let img = dynamic.ok_or(ImageError::Build)?;
        img.save_with_format(path, format)
            .map_err(|e| ImageError::Save(e.to_string()))
    }
}
