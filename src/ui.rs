#![cfg_attr(not(windows), allow(dead_code))]
//! Platform-independent UI: model, layout, hit-testing and software rendering
//! (tiny-skia shapes + fontdue text). The Windows shell (gui.rs) only blits
//! the pixels and forwards mouse input; on other OSes the same renderer is
//! used to produce PNG previews.

use crate::analyze::{Settings, Summary};
use crate::layout::{key_for, Mod};
use crate::schedule::VisNote;
use fontdue::{Font, FontSettings, Metrics};
use std::collections::HashMap;
use std::sync::Arc;
use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, Path, PathBuilder, Pixmap, Point, Rect, Shader,
    SpreadMode, Stroke, Transform,
};

pub const W: f32 = 1120.0;
pub const H: f32 = 700.0;
const LOOKAHEAD_US: f64 = 2_600_000.0;

// player states (same numbers as player.rs)
pub const ST_STOPPED: u8 = 0;
pub const ST_ARMED: u8 = 1;
pub const ST_PLAYING: u8 = 2;
pub const ST_PAUSED: u8 = 3;
pub const ST_LOST: u8 = 4;
pub const ST_DONE: u8 = 5;

// ---------------------------------------------------------------- colors
/// Pixel buffers on Windows are BGRA, so red/blue are swapped at the source.
fn col(hex: u32, a: f32) -> Color {
    let (r, g, b) = ((hex >> 16) as u8, (hex >> 8) as u8, hex as u8);
    let (r, b) = if cfg!(windows) { (b, r) } else { (r, b) };
    Color::from_rgba8(r, g, b, (a.clamp(0.0, 1.0) * 255.0) as u8)
}
const BG: u32 = 0x0B0E14;
const CARD: u32 = 0x131824;
const CARD2: u32 = 0x1A2030;
const BORDER: u32 = 0x232B3B;
const TEXT: u32 = 0xE7EBF3;
const SUB: u32 = 0x8B95A8;
const DIM: u32 = 0x5B6578;
const ACCENT: u32 = 0x7C6CFF;
const ACCENT2: u32 = 0x4FD1C5;
const GREEN: u32 = 0x3DDC97;
const AMBER: u32 = 0xF6AD55;
const RED: u32 = 0xFF6B6B;
const NOTE_WHITE: u32 = 0x4FD1C5; // plain keys
const NOTE_SHIFT: u32 = 0xA78BFA; // Shift (black keys)
const NOTE_CTRL: u32 = 0xF6AD55; // Ctrl (outer 27 keys)

fn note_color(pitch: u8) -> u32 {
    match key_for(pitch).map(|k| k.modifier) {
        Some(Mod::Shift) => NOTE_SHIFT,
        Some(Mod::Ctrl) => NOTE_CTRL,
        _ => NOTE_WHITE,
    }
}

// ---------------------------------------------------------------- model
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Wid {
    Open,
    Play,
    Stop,
    Seek,
    SpeedDn,
    SpeedUp,
    TransDn,
    TransUp,
    TolDn,
    TolUp,
    RateDn,
    RateUp,
    Pedal,
    Drums,
    Roblox,
    Vis,
}

pub struct Model {
    pub speed: f64,
    pub transpose: i32,
    /// None = automatic
    pub tol_ms: Option<i32>,
    pub rate: i32,
    pub pedal: bool,
    pub drums: bool,
    pub roblox_only: bool,
    pub vis_on: bool,
    pub file_name: Option<String>,
    pub summary: Option<Summary>,
    pub analyzing: bool,
    pub error: Option<String>,
    pub vis: Arc<Vec<VisNote>>,
    pub vis_maxdur: i64,
    pub state: u8,
    pub pos_us: i64,
    pub blocked: bool,
    pub hover: Option<Wid>,
    pub pressed: Option<Wid>,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            speed: 1.0,
            transpose: 0,
            tol_ms: None,
            rate: 200,
            pedal: true,
            drums: false,
            roblox_only: true,
            vis_on: true,
            file_name: None,
            summary: None,
            analyzing: false,
            error: None,
            vis: Arc::new(vec![]),
            vis_maxdur: 0,
            state: ST_STOPPED,
            pos_us: 0,
            blocked: false,
            hover: None,
            pressed: None,
        }
    }
}

impl Model {
    pub fn settings(&self) -> Settings {
        Settings {
            speed: self.speed,
            transpose: self.transpose,
            pedal: self.pedal,
            drums: self.drums,
            tolerance: self.tol_ms.map(|t| t as f64),
            max_rate: Some(self.rate as f64),
            ..Settings::default()
        }
    }
    pub fn auto_tol_ms(&self) -> i32 {
        let p = crate::schedule::Params::default();
        (p.tolerance as f64 / 1000.0).round() as i32
    }
    pub fn editable(&self) -> bool {
        matches!(self.state, ST_STOPPED | ST_DONE) && !self.analyzing
    }
    pub fn set_vis(&mut self, v: Vec<VisNote>) {
        self.vis_maxdur = v.iter().map(|n| n.end - n.start).max().unwrap_or(0);
        self.vis = Arc::new(v);
    }
    /// Apply a click. Returns true if analysis settings changed.
    pub fn click_setting(&mut self, w: Wid) -> bool {
        if !self.editable() && !matches!(w, Wid::Vis | Wid::Roblox) {
            return false;
        }
        match w {
            Wid::SpeedDn => self.speed = ((self.speed - 0.05) * 100.0).round() / 100.0,
            Wid::SpeedUp => self.speed = ((self.speed + 0.05) * 100.0).round() / 100.0,
            Wid::TransDn => self.transpose -= 1,
            Wid::TransUp => self.transpose += 1,
            Wid::TolDn => {
                self.tol_ms = match self.tol_ms {
                    None => Some(((self.auto_tol_ms() - 1) / 5 * 5).max(0)),
                    Some(0) => None, // below 0 -> back to automatic
                    Some(t) => Some(t - 5),
                }
            }
            Wid::TolUp => {
                self.tol_ms = match self.tol_ms {
                    None => Some((self.auto_tol_ms() / 5 + 1) * 5),
                    Some(t) => Some(t + 5),
                }
            }
            Wid::RateDn => self.rate -= 25,
            Wid::RateUp => self.rate += 25,
            Wid::Pedal => self.pedal = !self.pedal,
            Wid::Drums => self.drums = !self.drums,
            Wid::Roblox => {
                self.roblox_only = !self.roblox_only;
                return false;
            }
            Wid::Vis => {
                self.vis_on = !self.vis_on;
                return false;
            }
            _ => return false,
        }
        self.speed = self.speed.clamp(0.25, 3.0);
        self.transpose = self.transpose.clamp(-24, 24);
        self.tol_ms = self.tol_ms.map(|t| t.clamp(0, 150));
        self.rate = self.rate.clamp(0, 1000);
        true
    }
    pub fn length_us(&self) -> i64 {
        self.summary.as_ref().map(|s| s.length_us).unwrap_or(0)
    }
}

// ---------------------------------------------------------------- layout
#[derive(Clone, Copy, Debug)]
pub struct R {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}
impl R {
    const fn new(x: f32, y: f32, w: f32, h: f32) -> R {
        R { x, y, w, h }
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
    fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }
    fn cx(&self) -> f32 {
        self.x + self.w / 2.0
    }
}

const M: f32 = 20.0;
const RIGHT_W: f32 = 340.0;
const VIS_CARD: R = R::new(M, 72.0, W - M * 3.0 - RIGHT_W, H - 72.0 - M - 84.0 - 12.0);
const TRANSPORT: R = R::new(M, H - M - 84.0, W - M * 3.0 - RIGHT_W, 84.0);
const SET_CARD: R = R::new(W - M - RIGHT_W, 72.0, RIGHT_W, 370.0);
const STAT_CARD: R = R::new(W - M - RIGHT_W, 72.0 + 370.0 + 12.0, RIGHT_W, H - M - (72.0 + 370.0 + 12.0));
const OPEN_BTN: R = R::new(W - M - 150.0, 15.0, 150.0, 38.0);
const ROW0: f32 = SET_CARD.y + 56.0;
const ROW_H: f32 = 42.0;
const TOG0: f32 = ROW0 + ROW_H * 4.0 + 10.0;
const TOG_H: f32 = 34.0;

fn play_btn() -> (f32, f32, f32) {
    (TRANSPORT.x + 46.0, TRANSPORT.cy(), 26.0)
}
fn stop_btn() -> (f32, f32, f32) {
    (TRANSPORT.x + 104.0, TRANSPORT.cy(), 19.0)
}
fn seek_bar() -> R {
    R::new(TRANSPORT.x + 200.0, TRANSPORT.cy() - 14.0, TRANSPORT.w - 200.0 - 76.0, 8.0)
}
fn stepper_rects(row: usize) -> (R, R) {
    let y = ROW0 + row as f32 * ROW_H + 6.0;
    let right = SET_CARD.x + SET_CARD.w - 18.0;
    (R::new(right - 124.0, y, 30.0, 30.0), R::new(right - 30.0, y, 30.0, 30.0))
}
fn toggle_row(i: usize) -> R {
    R::new(SET_CARD.x + 8.0, TOG0 + i as f32 * TOG_H, SET_CARD.w - 16.0, TOG_H)
}

/// Every clickable thing and where it is.
pub fn widgets() -> Vec<(R, Wid)> {
    let mut v = vec![(OPEN_BTN, Wid::Open)];
    let (px, py, pr) = play_btn();
    v.push((R::new(px - pr, py - pr, pr * 2.0, pr * 2.0), Wid::Play));
    let (sx, sy, sr) = stop_btn();
    v.push((R::new(sx - sr, sy - sr, sr * 2.0, sr * 2.0), Wid::Stop));
    let sb = seek_bar();
    v.push((R::new(sb.x - 6.0, sb.y - 10.0, sb.w + 12.0, sb.h + 20.0), Wid::Seek));
    let pairs = [(Wid::SpeedDn, Wid::SpeedUp), (Wid::TransDn, Wid::TransUp), (Wid::TolDn, Wid::TolUp), (Wid::RateDn, Wid::RateUp)];
    for (i, (dn, up)) in pairs.iter().enumerate() {
        let (a, b) = stepper_rects(i);
        v.push((a, *dn));
        v.push((b, *up));
    }
    for (i, w) in [Wid::Pedal, Wid::Drums, Wid::Roblox, Wid::Vis].iter().enumerate() {
        v.push((toggle_row(i), *w));
    }
    v
}

pub fn hit(x: f32, y: f32) -> Option<Wid> {
    widgets().into_iter().find(|(r, _)| r.contains(x, y)).map(|(_, w)| w)
}

/// Seek position (0..1) for a mouse x over the seek bar.
pub fn seek_fraction(x: f32) -> f64 {
    let sb = seek_bar();
    (((x - sb.x) / sb.w) as f64).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------- canvas
pub struct Fonts {
    regular: Font,
    bold: Font,
}

impl Fonts {
    pub fn load(regular: &[u8], reg_index: u32, bold: &[u8], bold_index: u32) -> Result<Fonts, String> {
        let mk = |b: &[u8], i: u32| {
            Font::from_bytes(b, FontSettings { collection_index: i, scale: 40.0, ..FontSettings::default() })
                .map_err(|e| e.to_string())
        };
        Ok(Fonts { regular: mk(regular, reg_index)?, bold: mk(bold, bold_index)? })
    }
}

pub struct Canvas {
    pub pm: Pixmap,
    pub s: f32,
    fonts: Fonts,
    cache: HashMap<(bool, char, u32), (Metrics, Vec<u8>)>,
    /// static layer (everything that does not move every frame)
    bg: Option<Pixmap>,
    bg_sig: u64,
}

fn rrect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    if r < 0.5 {
        return Some(PathBuilder::from_rect(Rect::from_xywh(x, y, w, h)?));
    }
    let k = 0.5523 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

fn solid(c: Color) -> Paint<'static> {
    Paint { shader: Shader::SolidColor(c), anti_alias: true, ..Paint::default() }
}

fn vgrad(y0: f32, y1: f32, c0: Color, c1: Color) -> Paint<'static> {
    let sh = LinearGradient::new(
        Point::from_xy(0.0, y0),
        Point::from_xy(0.0, y1),
        vec![GradientStop::new(0.0, c0), GradientStop::new(1.0, c1)],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or(Shader::SolidColor(c0));
    Paint { shader: sh, anti_alias: true, ..Paint::default() }
}

fn hgrad(x0: f32, x1: f32, c0: Color, c1: Color) -> Paint<'static> {
    let sh = LinearGradient::new(
        Point::from_xy(x0, 0.0),
        Point::from_xy(x1, 0.0),
        vec![GradientStop::new(0.0, c0), GradientStop::new(1.0, c1)],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or(Shader::SolidColor(c0));
    Paint { shader: sh, anti_alias: true, ..Paint::default() }
}

impl Canvas {
    pub fn new(scale: f32, fonts: Fonts) -> Canvas {
        let pm = Pixmap::new((W * scale).round() as u32, (H * scale).round() as u32).unwrap();
        Canvas { pm, s: scale, fonts, cache: HashMap::new(), bg: None, bg_sig: 0 }
    }
    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.s).abs() > 0.001 {
            self.s = scale;
            self.pm = Pixmap::new((W * scale).round() as u32, (H * scale).round() as u32).unwrap();
            self.cache.clear();
            self.bg = None;
        }
    }
    fn tf(&self) -> Transform {
        Transform::from_scale(self.s, self.s)
    }
    fn fill(&mut self, p: Option<Path>, paint: &Paint) {
        if let Some(p) = p {
            let t = self.tf();
            self.pm.fill_path(&p, paint, FillRule::Winding, t, None);
        }
    }
    fn rr(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Color) {
        self.fill(rrect(x, y, w, h, r), &solid(c));
    }
    fn rr_paint(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, paint: &Paint) {
        self.fill(rrect(x, y, w, h, r), paint);
    }
    fn rr_stroke(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Color, width: f32) {
        if let Some(p) = rrect(x, y, w, h, r) {
            let t = self.tf();
            self.pm.stroke_path(&p, &solid(c), &Stroke { width, ..Stroke::default() }, t, None);
        }
    }
    /// Axis-aligned rectangle written straight into the pixel buffer
    /// (no path pipeline: thousands of these per frame stay cheap).
    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        let s = self.s;
        let (pw, ph) = (self.pm.width() as i32, self.pm.height() as i32);
        let x0 = ((x * s).round() as i32).clamp(0, pw);
        let y0 = ((y * s).round() as i32).clamp(0, ph);
        let x1 = (((x + w) * s).round() as i32).clamp(0, pw);
        let y1 = (((y + h) * s).round() as i32).clamp(0, ph);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let a = c.alpha();
        let px = [
            (c.red() * a * 255.0 + 0.5) as u8,
            (c.green() * a * 255.0 + 0.5) as u8,
            (c.blue() * a * 255.0 + 0.5) as u8,
            (a * 255.0 + 0.5) as u8,
        ];
        let inv = ((1.0 - a) * 256.0) as u32;
        let data = self.pm.data_mut();
        for yy in y0..y1 {
            let row = &mut data[((yy * pw + x0) * 4) as usize..((yy * pw + x1) * 4) as usize];
            if px[3] == 255 {
                for d in row.chunks_exact_mut(4) {
                    d.copy_from_slice(&px);
                }
            } else {
                for d in row.chunks_exact_mut(4) {
                    d[0] = px[0] + ((d[0] as u32 * inv) >> 8) as u8;
                    d[1] = px[1] + ((d[1] as u32 * inv) >> 8) as u8;
                    d[2] = px[2] + ((d[2] as u32 * inv) >> 8) as u8;
                    d[3] = px[3] + ((d[3] as u32 * inv) >> 8) as u8;
                }
            }
        }
    }
    fn circle(&mut self, cx: f32, cy: f32, r: f32, paint: &Paint) {
        self.fill(PathBuilder::from_circle(cx, cy, r), paint);
    }
    fn poly(&mut self, pts: &[(f32, f32)], c: Color) {
        let mut pb = PathBuilder::new();
        pb.move_to(pts[0].0, pts[0].1);
        for p in &pts[1..] {
            pb.line_to(p.0, p.1);
        }
        pb.close();
        self.fill(pb.finish(), &solid(c));
    }

    fn ensure_glyph(&mut self, bold: bool, ch: char, px: f32) -> (bool, char, u32) {
        let key = (bold, ch, (px * 4.0) as u32);
        if !self.cache.contains_key(&key) {
            let f = if bold { &self.fonts.bold } else { &self.fonts.regular };
            let g = f.rasterize(ch, px);
            self.cache.insert(key, g);
        }
        key
    }
    fn glyph(&mut self, bold: bool, ch: char, px: f32) -> &(Metrics, Vec<u8>) {
        let key = self.ensure_glyph(bold, ch, px);
        &self.cache[&key]
    }

    pub fn measure(&mut self, size: f32, bold: bool, txt: &str) -> f32 {
        let px = size * self.s;
        let mut w = 0.0;
        for ch in txt.chars() {
            w += self.glyph(bold, ch, px).0.advance_width;
        }
        w / self.s
    }

    /// Draw text with its top at `y` (logical units). Returns the width.
    pub fn text(&mut self, x: f32, y: f32, size: f32, bold: bool, c: Color, txt: &str) -> f32 {
        let px = size * self.s;
        let f = if bold { &self.fonts.bold } else { &self.fonts.regular };
        let asc = f.horizontal_line_metrics(px).map(|m| m.ascent).unwrap_or(px * 0.9);
        let base = (y * self.s + asc).round();
        let mut pen = x * self.s;
        let (pw, ph) = (self.pm.width() as i32, self.pm.height() as i32);
        let (cr, cg, cb, ca) = (c.red(), c.green(), c.blue(), c.alpha());
        for ch in txt.chars() {
            let key = self.ensure_glyph(bold, ch, px);
            let (m, bmp) = {
                let g = &self.cache[&key];
                (g.0, &g.1)
            };
            let gx = (pen + m.xmin as f32).round() as i32;
            let gy = (base - (m.height as f32 + m.ymin as f32)) as i32;
            let data = self.pm.data_mut();
            for row in 0..m.height as i32 {
                let yy = gy + row;
                if yy < 0 || yy >= ph {
                    continue;
                }
                for col_ in 0..m.width as i32 {
                    let xx = gx + col_;
                    if xx < 0 || xx >= pw {
                        continue;
                    }
                    let cov = bmp[(row * m.width as i32 + col_) as usize] as f32 / 255.0;
                    if cov <= 0.0 {
                        continue;
                    }
                    let a = cov * ca;
                    let i = ((yy * pw + xx) * 4) as usize;
                    let inv = 1.0 - a;
                    data[i] = (cr * a * 255.0 + data[i] as f32 * inv) as u8;
                    data[i + 1] = (cg * a * 255.0 + data[i + 1] as f32 * inv) as u8;
                    data[i + 2] = (cb * a * 255.0 + data[i + 2] as f32 * inv) as u8;
                    data[i + 3] = (a * 255.0 + data[i + 3] as f32 * inv) as u8;
                }
            }
            pen += m.advance_width;
        }
        (pen - x * self.s) / self.s
    }
    fn text_c(&mut self, cx: f32, cy: f32, size: f32, bold: bool, c: Color, txt: &str) {
        let w = self.measure(size, bold, txt);
        self.text(cx - w / 2.0, cy - size * 0.68, size, bold, c, txt);
    }
    fn text_r(&mut self, right: f32, y: f32, size: f32, bold: bool, c: Color, txt: &str) {
        let w = self.measure(size, bold, txt);
        self.text(right - w, y, size, bold, c, txt);
    }
}

fn mmss(us: i64) -> String {
    let s = (us.max(0) / 1_000_000) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

// ---------------------------------------------------------------- keyboard geometry
fn is_black(p: u8) -> bool {
    matches!(p % 12, 1 | 3 | 6 | 8 | 10)
}
/// x position and width of a key inside the keyboard rect.
fn key_x(p: u8, kb: &R) -> (f32, f32) {
    let ww = kb.w / 52.0;
    let whites_before = (21..p).filter(|&q| !is_black(q)).count() as f32;
    if is_black(p) {
        let bw = ww * 0.62;
        (kb.x + whites_before * ww - bw / 2.0, bw)
    } else {
        (kb.x + whites_before * ww, ww)
    }
}

// ---------------------------------------------------------------- render
/// Everything that changes the static layer.
fn static_sig(cv: &Canvas, m: &Model) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (m.speed.to_bits(), m.transpose, m.tol_ms, m.rate).hash(&mut h);
    (m.pedal, m.drums, m.roblox_only, m.vis_on, &m.file_name, m.analyzing, &m.error).hash(&mut h);
    (m.state, m.hover, cv.s.to_bits()).hash(&mut h);
    if let Some(su) = &m.summary {
        (su.kept, su.dropped, su.tempo_changes, su.max_late_us, su.ok).hash(&mut h);
    }
    h.finish()
}

pub fn render(cv: &mut Canvas, m: &Model) {
    let sig = static_sig(cv, m);
    if cv.bg.is_none() || cv.bg_sig != sig {
        render_static(cv, m);
        cv.bg = Some(cv.pm.clone());
        cv.bg_sig = sig;
    } else {
        let bg = cv.bg.as_ref().unwrap();
        cv.pm.data_mut().copy_from_slice(bg.data());
    }
    visualizer_dynamic(cv, m);
    transport(cv, m);
}

fn render_static(cv: &mut Canvas, m: &Model) {
    cv.pm.fill(col(BG, 1.0));
    header(cv, m);
    visualizer_static(cv, m);
    card(cv, TRANSPORT);
    settings_card(cv, m);
    stats_card(cv, m);
}

fn card(cv: &mut Canvas, r: R) {
    cv.rr(r.x, r.y, r.w, r.h, 16.0, col(CARD, 1.0));
    cv.rr_stroke(r.x + 0.5, r.y + 0.5, r.w - 1.0, r.h - 1.0, 16.0, col(BORDER, 1.0), 1.0);
}

fn header(cv: &mut Canvas, m: &Model) {
    // logo mark: gradient rounded square with a note bar
    let p = hgrad(M, M + 34.0, col(ACCENT, 1.0), col(ACCENT2, 1.0));
    cv.rr_paint(M, 17.0, 34.0, 34.0, 10.0, &p);
    for (i, h) in [14.0f32, 20.0, 10.0].iter().enumerate() {
        let x = M + 8.0 + i as f32 * 7.0;
        cv.rr(x, 17.0 + 26.0 - h, 4.5, *h, 2.0, col(0xFFFFFF, 0.92));
    }
    let tw = cv.text(M + 46.0, 17.0, 21.0, true, col(TEXT, 1.0), "keyblox");
    cv.text(M + 46.0 + tw + 10.0, 22.5, 13.0, false, col(SUB, 1.0), "Roblox 88키 MIDI 자동 연주");

    // open button
    let hov = m.hover == Some(Wid::Open);
    let b = OPEN_BTN;
    let p = hgrad(b.x, b.x + b.w, col(ACCENT, if hov { 1.0 } else { 0.9 }), col(0x9A6BFF, if hov { 1.0 } else { 0.9 }));
    cv.rr_paint(b.x, b.y, b.w, b.h, 11.0, &p);
    cv.text_c(b.cx(), b.cy() + 1.0, 14.0, true, col(0xFFFFFF, 1.0), "MIDI 파일 열기");

    // file pill
    let name = m.file_name.clone().unwrap_or_else(|| "파일 없음 · 창에 끌어다 놓아도 됩니다".into());
    let name = ellipsize(cv, &name, 13.0, 360.0);
    let w = cv.measure(13.0, false, &name) + 44.0;
    let x = b.x - 12.0 - w;
    cv.rr(x, b.y + 3.0, w, b.h - 6.0, 16.0, col(CARD2, 1.0));
    cv.rr_stroke(x + 0.5, b.y + 3.5, w - 1.0, b.h - 7.0, 16.0, col(BORDER, 1.0), 1.0);
    cv.circle(x + 18.0, b.cy(), 4.0, &solid(col(if m.file_name.is_some() { GREEN } else { DIM }, 1.0)));
    cv.text(x + 30.0, b.y + 10.0, 13.0, false, col(if m.file_name.is_some() { TEXT } else { SUB }, 1.0), &name);
}

fn ellipsize(cv: &mut Canvas, s: &str, size: f32, max: f32) -> String {
    if cv.measure(size, false, s) <= max {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut n = chars.len();
    while n > 1 {
        n -= 1;
        let t: String = chars[..n].iter().collect::<String>() + "…";
        if cv.measure(size, false, &t) <= max {
            return t;
        }
    }
    "…".into()
}

struct VisGeo {
    area: R,
    kb: R,
}
fn vis_geo() -> VisGeo {
    let c = VIS_CARD;
    let inner = R::new(c.x + 12.0, c.y + 12.0, c.w - 24.0, c.h - 24.0);
    let kb_h = 92.0;
    VisGeo {
        kb: R::new(inner.x, inner.y + inner.h - kb_h, inner.w, kb_h),
        area: R::new(inner.x, inner.y, inner.w, inner.h - kb_h - 4.0),
    }
}

fn white_key(cv: &mut Canvas, p: u8, kb: &R, c: Color) {
    let (x, w) = key_x(p, kb);
    cv.rr(x + 0.5, kb.y, w - 1.0, kb.h, 4.0, c);
}
fn black_key(cv: &mut Canvas, p: u8, kb: &R, c: Color) {
    let (x, w) = key_x(p, kb);
    cv.rr(x, kb.y - 1.0, w, kb.h * 0.62, 3.0, c);
}

fn visualizer_static(cv: &mut Canvas, _m: &Model) {
    card(cv, VIS_CARD);
    let VisGeo { area, kb } = vis_geo();
    let bgp = vgrad(area.y, area.y + area.h, col(0x0A0D14, 1.0), col(0x0F1320, 1.0));
    cv.rr_paint(area.x, area.y, area.w, area.h + 4.0, 10.0, &bgp);
    for p in 21..=108u8 {
        if !is_black(p) {
            white_key(cv, p, &kb, col(0xE9EDF3, 1.0));
        }
    }
    for p in 21..=108u8 {
        if is_black(p) {
            black_key(cv, p, &kb, col(0x1B2130, 1.0));
        }
    }
    for p in (24..=108).step_by(12) {
        let (x, w) = key_x(p, &kb);
        cv.text_c(x + w / 2.0, kb.y + kb.h - 8.0, 9.5, false, col(0x7A8394, 1.0), &format!("C{}", p / 12 - 1));
    }
}

/// Lane background region that the notes are drawn over (restored from the
/// static layer every frame, so only notes / lit keys / glow are drawn here).
fn visualizer_dynamic(cv: &mut Canvas, m: &Model) {
    let VisGeo { area, kb } = vis_geo();
    for p in (24..=108).step_by(12) {
        let (x, _) = key_x(p, &kb);
        cv.rect(x, area.y + 6.0, 1.0, area.h - 6.0, col(0xFFFFFF, 0.045));
    }
    let now = m.pos_us;
    let pps = area.h as f64 / LOOKAHEAD_US;
    let hit_y = area.y + area.h;
    let mut lit: [Option<u32>; 128] = [None; 128];

    if m.vis_on && !m.vis.is_empty() {
        let v = m.vis.clone();
        let lo = v.partition_point(|n| n.start < now - m.vis_maxdur);
        let hi = v.partition_point(|n| (n.start as f64) < now as f64 + LOOKAHEAD_US);
        let visible = &v[lo..hi];
        let kept_n = visible.iter().filter(|n| n.kept && n.end >= now).count();
        let fancy = kept_n < 400; // rounded, shaded notes unless the screen is flooded
        // removed notes: faint ghosts
        for n in visible.iter().filter(|n| !n.kept && n.end >= now) {
            let (x, w) = key_x(n.pitch, &kb);
            let yb = (hit_y - ((n.start - now) as f64 * pps) as f32).min(hit_y);
            let yt = (hit_y - ((n.end - now) as f64 * pps) as f32).max(area.y);
            if yb - yt >= 1.0 {
                cv.rect(x + 2.0, yt, w - 4.0, yb - yt, col(0x8A96B4, 0.13));
            }
        }
        // struck notes (white keys first so black-key notes sit on top)
        for black in [false, true] {
            for n in visible.iter().filter(|n| n.kept && is_black(n.pitch) == black) {
                if n.end < now - 30_000 {
                    continue;
                }
                let (x, w) = key_x(n.pitch, &kb);
                let yb = (hit_y - ((n.start - now) as f64 * pps) as f32).min(hit_y);
                let yt = (hit_y - ((n.end.max(n.start + 40_000) - now) as f64 * pps) as f32).max(area.y);
                if yb <= area.y || yb - yt < 1.0 {
                    continue;
                }
                let nc = note_color(n.pitch);
                if n.start <= now && now < n.end.max(n.start + 60_000) {
                    lit[n.pitch as usize] = Some(nc);
                }
                let h = (yb - yt).max(4.0);
                if fancy {
                    let p = vgrad(yt, yb, col(nc, 0.78), col(nc, 1.0));
                    cv.rr_paint(x + 1.5, yt, w - 3.0, h, 4.0, &p);
                } else {
                    cv.rect(x + 1.5, yt, w - 3.0, h, col(nc, 0.95));
                    cv.rect(x + 1.5, yt, w - 3.0, h.min(2.0), col(0xFFFFFF, 0.35));
                }
            }
        }
    }

    // hit line glow
    let glow = vgrad(hit_y - 18.0, hit_y, col(ACCENT, 0.0), col(ACCENT, 0.35));
    cv.rr_paint(area.x, hit_y - 18.0, area.w, 18.0, 0.0, &glow);
    cv.rect(area.x, hit_y - 1.5, area.w, 2.0, col(ACCENT, 0.9));

    // lit keys over the cached keyboard
    let mut redraw_black = [false; 128];
    for p in 21..=108u8 {
        if let (false, Some(nc)) = (is_black(p), lit[p as usize]) {
            white_key(cv, p, &kb, col(nc, 1.0));
            for q in [p.saturating_sub(1), p + 1] {
                if (21..=108).contains(&q) && is_black(q) {
                    redraw_black[q as usize] = true;
                }
            }
        }
    }
    for p in 21..=108u8 {
        if is_black(p) {
            if let Some(nc) = lit[p as usize] {
                black_key(cv, p, &kb, col(nc, 1.0));
            } else if redraw_black[p as usize] {
                black_key(cv, p, &kb, col(0x1B2130, 1.0));
            }
        }
    }

    // overlays
    if m.vis.is_empty() || !m.vis_on {
        let msg = if m.analyzing {
            "분석 중…"
        } else if !m.vis_on {
            "비주얼라이저 꺼짐 (CPU 절약)"
        } else {
            "MIDI 파일을 이 창에 끌어다 놓거나 오른쪽 위 버튼으로 여세요"
        };
        cv.text_c(area.cx(), area.cy(), 15.0, false, col(SUB, 1.0), msg);
    } else if m.analyzing {
        cv.rr(area.cx() - 70.0, area.y + 16.0, 140.0, 30.0, 15.0, col(CARD2, 0.95));
        cv.text_c(area.cx(), area.y + 31.0, 13.0, false, col(TEXT, 1.0), "다시 분석 중…");
    }
    // soft fade where notes enter at the top
    let fade = vgrad(area.y, area.y + 36.0, col(0x0A0D14, 1.0), col(0x0A0D14, 0.0));
    cv.rr_paint(area.x, area.y, area.w, 36.0, 0.0, &fade);
    // legend on a dark pill so notes never cover it
    let lw: f32 = ["흰건반", "Shift", "Ctrl", "뺀 음"].iter().map(|t| cv.measure(11.5, false, t) + 30.0).sum::<f32>() + 6.0;
    cv.rr(area.x + 8.0, area.y + 8.0, lw, 24.0, 12.0, col(0x0A0D14, 0.85));
    let mut lx = area.x + 18.0;
    for (c, t, a) in [(NOTE_WHITE, "흰건반", 1.0), (NOTE_SHIFT, "Shift", 1.0), (NOTE_CTRL, "Ctrl", 1.0), (0x8A96B4, "뺀 음", 0.35)] {
        cv.rr(lx, area.y + 15.0, 10.0, 10.0, 3.0, col(c, a));
        lx += 16.0 + cv.text(lx + 16.0, area.y + 12.0, 11.5, false, col(SUB, 1.0), t) + 14.0;
    }
}

fn transport(cv: &mut Canvas, m: &Model) {
    let can = m.summary.is_some() && !m.analyzing;
    // play / pause
    let (px, py, pr) = play_btn();
    let hov = m.hover == Some(Wid::Play) && can;
    let a = if can { 1.0 } else { 0.35 };
    let p = hgrad(px - pr, px + pr, col(ACCENT, a), col(0x9A6BFF, a));
    if hov {
        cv.circle(px, py, pr + 4.0, &solid(col(ACCENT, 0.18)));
    }
    cv.circle(px, py, pr, &p);
    let white = col(0xFFFFFF, if can { 1.0 } else { 0.6 });
    if matches!(m.state, ST_PLAYING | ST_ARMED) {
        cv.rr(px - 7.5, py - 9.0, 5.5, 18.0, 1.5, white);
        cv.rr(px + 2.0, py - 9.0, 5.5, 18.0, 1.5, white);
    } else {
        cv.poly(&[(px - 6.0, py - 10.0), (px + 11.0, py), (px - 6.0, py + 10.0)], white);
    }
    // stop
    let (sx, sy, sr) = stop_btn();
    let hov = m.hover == Some(Wid::Stop);
    cv.circle(sx, sy, sr, &solid(col(if hov { 0x283044 } else { CARD2 }, 1.0)));
    cv.rr(sx - 6.0, sy - 6.0, 12.0, 12.0, 2.5, col(TEXT, if can { 0.9 } else { 0.4 }));

    // times + seek bar
    let sb = seek_bar();
    let len = m.length_us();
    let frac = if len > 0 { (m.pos_us as f64 / len as f64).clamp(0.0, 1.0) } else { 0.0 } as f32;
    cv.text_r(sb.x - 14.0, sb.y - 6.0, 13.0, true, col(TEXT, 1.0), &mmss(m.pos_us));
    cv.text(sb.x + sb.w + 14.0, sb.y - 6.0, 13.0, false, col(SUB, 1.0), &mmss(len));
    cv.rr(sb.x, sb.y, sb.w, sb.h, 4.0, col(0x252D3F, 1.0));
    if frac > 0.0 {
        let p = hgrad(sb.x, sb.x + sb.w * frac, col(ACCENT, 1.0), col(ACCENT2, 1.0));
        cv.rr_paint(sb.x, sb.y, (sb.w * frac).max(8.0), sb.h, 4.0, &p);
    }
    if m.hover == Some(Wid::Seek) || m.pressed == Some(Wid::Seek) {
        cv.circle(sb.x + sb.w * frac, sb.y + sb.h / 2.0, 7.0, &solid(col(0xFFFFFF, 1.0)));
    }

    // state chip + hint
    let (txt, c) = if m.blocked {
        ("입력 차단됨 · 관리자 권한으로 실행해 보세요".to_string(), RED)
    } else {
        match m.state {
            ST_ARMED => ("Roblox 창을 클릭하면 시작합니다".into(), AMBER),
            ST_PLAYING => ("재생 중".into(), GREEN),
            ST_PAUSED => ("일시정지".into(), SUB),
            ST_LOST => ("Roblox 창이 앞에 없어 일시정지".into(), AMBER),
            ST_DONE => ("연주 완료".into(), ACCENT2),
            _ if m.analyzing => ("분석 중…".into(), SUB),
            _ if m.summary.is_some() => ("준비됨".into(), SUB),
            _ => ("파일을 열어 주세요".into(), DIM),
        }
    };
    let y = sb.y + 18.0;
    cv.circle(sb.x + 4.0, y + 8.0, 4.0, &solid(col(c, 1.0)));
    let w = cv.text(sb.x + 14.0, y, 12.5, true, col(c, 1.0), &txt);
    cv.text(sb.x + 14.0 + w + 14.0, y, 12.5, false, col(DIM, 1.0), "Roblox 안에서  F6 재생/일시정지 · F7 정지");
}

fn settings_card(cv: &mut Canvas, m: &Model) {
    let c = SET_CARD;
    card(cv, c);
    cv.text(c.x + 20.0, c.y + 18.0, 15.0, true, col(TEXT, 1.0), "설정");
    let ed = m.editable();
    if !ed {
        cv.text_r(c.x + c.w - 20.0, c.y + 20.0, 11.5, false, col(DIM, 1.0), "정지 상태에서 변경할 수 있어요");
    }
    let la = if ed { 1.0 } else { 0.45 };
    let label = |cv: &mut Canvas, row: usize, t: &str, sub: &str| {
        let y = ROW0 + row as f32 * ROW_H;
        cv.text(c.x + 20.0, y + 8.0, 13.5, false, col(TEXT, la), t);
        if !sub.is_empty() {
            cv.text(c.x + 20.0, y + 27.0, 10.5, false, col(DIM, la), sub);
        }
    };

    let rows: [(&str, &str, String, Wid, Wid); 4] = [
        ("재생 속도", "", format!("{:.2}x", m.speed), Wid::SpeedDn, Wid::SpeedUp),
        ("이조", "반음 단위", format!("{:+}", m.transpose), Wid::TransDn, Wid::TransUp),
        (
            "허용 지연",
            "화음을 펼쳐 칠 수 있는 최대 시간",
            match m.tol_ms {
                None => format!("자동 {}", m.auto_tol_ms()),
                Some(t) => format!("{t} ms"),
            },
            Wid::TolDn,
            Wid::TolUp,
        ),
        (
            "초당 최대 타건",
            "Roblox 렉 방지 · 0 = 무제한",
            if m.rate == 0 { "무제한".into() } else { format!("{}", m.rate) },
            Wid::RateDn,
            Wid::RateUp,
        ),
    ];
    for (i, (t, sub, val, dn, up)) in rows.iter().enumerate() {
        label(cv, i, t, sub);
        let (a, b) = stepper_rects(i);
        for (r, w, minus) in [(a, *dn, true), (b, *up, false)] {
            let hov = m.hover == Some(w) && ed;
            cv.rr(r.x, r.y, r.w, r.h, 9.0, col(if hov { 0x283044 } else { CARD2 }, 1.0));
            cv.rr(r.cx() - 5.5, r.cy() - 0.9, 11.0, 1.8, 0.9, col(TEXT, la));
            if !minus {
                cv.rr(r.cx() - 0.9, r.cy() - 5.5, 1.8, 11.0, 0.9, col(TEXT, la));
            }
        }
        cv.text_c((a.x + a.w + b.x) / 2.0, a.cy() + 1.0, 13.5, true, col(TEXT, la), val);
    }

    // divider + toggles
    cv.rect(c.x + 20.0, TOG0 - 6.0, c.w - 40.0, 1.0, col(BORDER, 1.0));
    let toggles = [
        ("서스테인 페달 → Space", m.pedal, Wid::Pedal, ed),
        ("드럼 채널 포함", m.drums, Wid::Drums, ed),
        ("Roblox 창에만 입력", m.roblox_only, Wid::Roblox, true),
        ("비주얼라이저", m.vis_on, Wid::Vis, true),
    ];
    for (i, (t, on, w, en)) in toggles.iter().enumerate() {
        let r = toggle_row(i);
        let a = if *en { 1.0 } else { 0.45 };
        if m.hover == Some(*w) && *en {
            cv.rr(r.x, r.y + 1.0, r.w, r.h - 2.0, 9.0, col(0x1A2030, 1.0));
        }
        cv.text(r.x + 12.0, r.y + 8.0, 13.5, false, col(TEXT, a), t);
        let (sw, sh) = (40.0, 22.0);
        let sx = r.x + r.w - 12.0 - sw;
        let sy = r.cy() - sh / 2.0;
        if *on {
            let p = hgrad(sx, sx + sw, col(ACCENT, a), col(0x9A6BFF, a));
            cv.rr_paint(sx, sy, sw, sh, 11.0, &p);
        } else {
            cv.rr(sx, sy, sw, sh, 11.0, col(0x2A3245, a));
        }
        let kx = if *on { sx + sw - 11.0 } else { sx + 11.0 };
        cv.circle(kx, r.cy(), 8.0, &solid(col(0xFFFFFF, if *on { 1.0 } else { 0.8 } * a)));
    }
}

fn stats_card(cv: &mut Canvas, m: &Model) {
    let c = STAT_CARD;
    card(cv, c);
    cv.text(c.x + 20.0, c.y + 18.0, 15.0, true, col(TEXT, 1.0), "분석");
    if let Some(e) = &m.error {
        let e = ellipsize(cv, e, 12.5, c.w - 40.0);
        cv.text(c.x + 20.0, c.y + 56.0, 12.5, false, col(RED, 1.0), &e);
        return;
    }
    let Some(s) = &m.summary else {
        cv.text(c.x + 20.0, c.y + 56.0, 12.5, false, col(SUB, 1.0), if m.analyzing { "분석 중…" } else { "파일을 열면 결과가 여기에 표시됩니다" });
        return;
    };
    // badge
    let (bt, bc) = if s.ok { ("✓ 검증 통과", GREEN) } else { ("검증 실패", RED) };
    let bw = cv.measure(11.5, true, bt) + 20.0;
    cv.rr(c.x + c.w - 20.0 - bw, c.y + 16.0, bw, 22.0, 11.0, col(bc, 0.14));
    cv.text_c(c.x + c.w - 20.0 - bw / 2.0, c.y + 27.5, 11.5, true, col(bc, 1.0), bt);

    let total = (s.kept + s.dropped).max(1);
    let pct = 100.0 * s.kept as f64 / total as f64;
    let y = c.y + 50.0;
    let w = cv.text(c.x + 20.0, y, 30.0, true, col(TEXT, 1.0), &format!("{pct:.1}"));
    cv.text(c.x + 20.0 + w + 2.0, y + 13.0, 15.0, true, col(SUB, 1.0), "%");
    cv.text(c.x + 20.0 + w + 26.0, y + 15.0, 12.0, false, col(SUB, 1.0), "원래 자리에서 연주되는 음");
    // ratio bar
    let by = y + 46.0;
    let bwid = c.w - 40.0;
    cv.rr(c.x + 20.0, by, bwid, 6.0, 3.0, col(0x252D3F, 1.0));
    let p = hgrad(c.x + 20.0, c.x + 20.0 + bwid, col(ACCENT, 1.0), col(ACCENT2, 1.0));
    cv.rr_paint(c.x + 20.0, by, (bwid * pct as f32 / 100.0).max(6.0), 6.0, 3.0, &p);

    let cells = [
        ("연주", thousands(s.kept)),
        ("뺀 음", thousands(s.dropped)),
        ("템포 이벤트", thousands(s.tempo_changes)),
        ("최대 지연", format!("{:.0} ms", s.max_late_us as f64 / 1000.0)),
    ];
    let cw = (c.w - 40.0) / 4.0;
    for (i, (k, v)) in cells.iter().enumerate() {
        let x = c.x + 20.0 + i as f32 * cw;
        cv.text(x, by + 20.0, 11.0, false, col(DIM, 1.0), k);
        cv.text(x, by + 37.0, 14.5, true, col(TEXT, 1.0), v);
    }
}
