#![cfg_attr(not(windows), allow(dead_code))]
//! Platform-independent UI: model, layout, hit-testing and software rendering
//! (tiny-skia shapes + fontdue text). The Windows shell (gui.rs) only blits
//! the pixels and forwards mouse input; on other OSes the same renderer is
//! used to produce PNG previews.

use crate::analyze::{Settings, Summary};
use crate::layout::{key_for, Mod};
use crate::library::{Row, RowKind};
use crate::schedule::VisNote;
use fontdue::{Font, FontSettings, Metrics};
use std::collections::HashMap;
use std::sync::Arc;
use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, Path, PathBuilder, Pixmap, Point, Rect, Shader,
    SpreadMode, Stroke, Transform,
};

pub const W: f32 = 1120.0 + SIDE_W + M;
pub const H: f32 = 784.0;
const LOOKAHEAD_US: f64 = 2_600_000.0;

// player states (same numbers as player.rs)
pub const ST_STOPPED: u8 = 0;
pub const ST_ARMED: u8 = 1;
pub const ST_PLAYING: u8 = 2;
pub const ST_PAUSED: u8 = 3;
pub const ST_LOST: u8 = 4;
pub const ST_DONE: u8 = 5;
pub const ST_COUNT: u8 = 6;

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
    WinDn,
    WinUp,
    HumanDn,
    HumanUp,
    CountDn,
    CountUp,
    /// Roblox frame-rate choice (index into FPS_CHOICES)
    Fps(u8),
    /// BPM / multiplier switch of the speed row
    SpeedUnit,
    Pedal,
    Roblox,
    Vis,
    AddFile,
    AddFolder,
    /// a playable row in the song list (row index)
    Song(usize),
    /// the remove button of a file row or folder header (row index)
    SongDel(usize),
    /// the value box of settings row `i` (type a value)
    Field(usize),
}

/// A settings value being typed into.
pub struct Edit {
    pub row: usize,
    pub text: String,
    /// the whole text is selected: the next character replaces it
    pub fresh: bool,
}

/// Roblox max frame rates offered; the Shift/Ctrl wait follows the choice.
pub const FPS_CHOICES: [u32; 5] = [60, 120, 144, 165, 240];

pub struct Model {
    /// index into FPS_CHOICES
    pub fps_idx: usize,
    pub speed: f64,
    /// single-tempo songs: set speed as BPM (true) or as a multiplier
    pub speed_bpm: bool,
    pub transpose: i32,
    /// None = automatic
    pub tol_ms: Option<i32>,
    /// strikes allowed per `rate_win_ms` (0 = unlimited)
    pub rate: i32,
    pub rate_win_ms: i32,
    /// 0 = off, 1..=3 (humanize::LEVEL_NAMES)
    pub human: u8,
    /// seconds to wait before playback starts (0 = off)
    pub countdown: i32,
    pub pedal: bool,
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
    /// time left in the countdown (state ST_COUNT)
    pub count_left_us: i64,
    pub blocked: bool,
    pub hover: Option<Wid>,
    pub pressed: Option<Wid>,
    /// sidebar rows (set with `set_songs`)
    pub songs: Vec<Row>,
    songs_ver: u64,
    /// row index of the open file
    pub song_sel: Option<usize>,
    /// first visible row
    pub song_scroll: usize,
    pub edit: Option<Edit>,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            fps_idx: 0,
            speed: 1.0,
            speed_bpm: true,
            transpose: 0,
            tol_ms: None,
            rate: 20,
            rate_win_ms: 100,
            human: 0,
            countdown: 3,
            pedal: true,
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
            count_left_us: 0,
            blocked: false,
            hover: None,
            pressed: None,
            songs: vec![],
            songs_ver: 0,
            song_sel: None,
            song_scroll: 0,
            edit: None,
        }
    }
}

impl Model {
    pub fn settings(&self) -> Settings {
        Settings {
            speed: self.speed,
            transpose: self.transpose,
            pedal: self.pedal,
            tolerance: self.tol_ms.map(|t| t as f64),
            rate_count: Some(self.rate as usize),
            rate_window: Some(self.rate_win_ms as f64),
            human: self.human,
            dwell: Some(self.dwell_us() as f64 / 1000.0),
            ..Settings::default()
        }
    }
    /// Shift/Ctrl wait for the chosen Roblox frame rate
    fn dwell_us(&self) -> i64 {
        crate::schedule::dwell_for_fps(FPS_CHOICES[self.fps_idx] as f64)
    }
    pub fn auto_tol_ms(&self) -> i32 {
        (crate::schedule::auto_tolerance(self.dwell_us()) as f64 / 1000.0).round() as i32
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
            // single-tempo songs step by whole BPM, others by 0.05x
            Wid::SpeedDn | Wid::SpeedUp => {
                let d = if w == Wid::SpeedUp { 1.0 } else { -1.0 };
                self.speed = match self.bpm_basis() {
                    Some(b) => ((b * self.speed).round() + d) / b,
                    None => ((self.speed + d * 0.05) * 100.0).round() / 100.0,
                }
            }
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
            Wid::RateDn => self.rate -= 1,
            Wid::RateUp => self.rate += 1,
            Wid::WinDn => self.rate_win_ms -= 10,
            Wid::WinUp => self.rate_win_ms += 10,
            Wid::HumanDn => self.human = self.human.saturating_sub(1),
            Wid::HumanUp => self.human = (self.human + 1).min(crate::humanize::LEVEL_NAMES.len() as u8 - 1),
            Wid::CountDn | Wid::CountUp => {
                self.countdown = (self.countdown + if w == Wid::CountUp { 1 } else { -1 }).clamp(0, 10);
                return false; // playback only, not analysis
            }
            Wid::SpeedUnit => {
                self.speed_bpm = !self.speed_bpm;
                return false; // same speed, shown another way
            }
            Wid::Fps(i) => self.fps_idx = (i as usize).min(FPS_CHOICES.len() - 1),
            Wid::Pedal => self.pedal = !self.pedal,
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
        self.clamp_settings();
        true
    }
    fn clamp_settings(&mut self) {
        self.speed = self.speed.clamp(0.25, 3.0);
        self.transpose = self.transpose.clamp(-24, 24);
        self.tol_ms = self.tol_ms.map(|t| t.clamp(0, 150));
        self.rate = self.rate.clamp(0, 200);
        self.rate_win_ms = self.rate_win_ms.clamp(10, 1000);
    }
    /// Open settings row `row` for typing, its current value selected.
    pub fn begin_edit(&mut self, row: usize) {
        if !self.editable() {
            return;
        }
        let text = match row {
            0 => match self.bpm_basis() {
                Some(b) => bpm_text(b * self.speed),
                None => format!("{:.2}", self.speed),
            },
            1 => self.transpose.to_string(),
            2 => self.tol_ms.map(|t| t.to_string()).unwrap_or_default(),
            3 => self.rate.to_string(),
            4 => self.rate_win_ms.to_string(),
            5 => self.human.to_string(),
            _ => self.countdown.to_string(),
        };
        self.edit = Some(Edit { row, text, fresh: true });
    }
    pub fn edit_char(&mut self, c: char) {
        if let Some(e) = &mut self.edit {
            if !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+')) {
                return;
            }
            if e.fresh {
                e.text.clear();
                e.fresh = false;
            }
            if e.text.len() < 7 {
                e.text.push(c);
            }
        }
    }
    pub fn edit_backspace(&mut self) {
        if let Some(e) = &mut self.edit {
            if e.fresh {
                e.text.clear();
            }
            e.text.pop();
            e.fresh = false;
        }
    }
    pub fn cancel_edit(&mut self) {
        self.edit = None;
    }
    /// Apply the typed value (clamped to the row's range). Returns true if
    /// analysis settings changed; text that is not a number changes nothing.
    pub fn commit_edit(&mut self) -> bool {
        let Some(e) = self.edit.take() else { return false };
        if !self.editable() {
            return false;
        }
        let t = e.text.trim();
        let before = (self.speed, self.transpose, self.tol_ms, self.rate, self.rate_win_ms, self.human);
        if e.row == 2 && t.is_empty() {
            self.tol_ms = None; // empty = automatic
        } else {
            let Ok(v) = t.parse::<f64>() else { return false };
            if !v.is_finite() {
                return false;
            }
            let n = v.round().clamp(-10_000.0, 10_000.0) as i32;
            match e.row {
                0 => {
                    self.speed = match self.bpm_basis() {
                        Some(b) => v / b,
                        None => (v * 100.0).round() / 100.0,
                    }
                }
                1 => self.transpose = n,
                2 => self.tol_ms = Some(n),
                3 => self.rate = n,
                4 => self.rate_win_ms = n,
                6 => self.countdown = n.clamp(0, 10),
                _ => self.human = n.clamp(0, crate::humanize::LEVEL_NAMES.len() as i32 - 1) as u8,
            }
            self.clamp_settings();
        }
        before != (self.speed, self.transpose, self.tol_ms, self.rate, self.rate_win_ms, self.human)
    }
    /// A different song was opened: speed/tempo is kept per song only.
    pub fn open_song(&mut self, name: String) {
        self.file_name = Some(name);
        self.summary = None;
        self.speed = 1.0;
        self.set_vis(vec![]);
    }
    /// Original BPM of the loaded song, when it uses a single tempo.
    pub fn base_bpm(&self) -> Option<f64> {
        self.summary.as_ref().and_then(|s| s.bpm)
    }
    /// The BPM that speed is shown and set against, when in BPM mode.
    fn bpm_basis(&self) -> Option<f64> {
        self.base_bpm().filter(|_| self.speed_bpm)
    }
    pub fn speed_text(&self) -> String {
        match self.bpm_basis() {
            Some(b) => format!("{} BPM", bpm_text(b * self.speed)),
            None => format!("{:.2}x", self.speed),
        }
    }
    pub fn set_songs(&mut self, rows: Vec<Row>) {
        self.songs = rows;
        self.songs_ver += 1;
        self.scroll_songs(0);
    }
    /// Scroll the song list by `rows` (negative = up), clamped.
    pub fn scroll_songs(&mut self, rows: i32) {
        let max = self.songs.len().saturating_sub(list_rows());
        self.song_scroll = (self.song_scroll as i64 + rows as i64).clamp(0, max as i64) as usize;
    }
    pub fn length_us(&self) -> i64 {
        self.summary.as_ref().map(|s| s.length_us).unwrap_or(0)
    }
}

/// Hold-to-repeat timing for the -/+ buttons: given when the previous step
/// fired (ms since press, 0 = the press itself), when the next one fires.
/// Waits 400 ms, then repeats every 120 ms, speeding up to 30 ms over 2 s.
pub fn next_repeat(prev_ms: u64) -> u64 {
    if prev_ms < 400 {
        return 400;
    }
    let t = (prev_ms - 400).min(2000);
    prev_ms + 120 - 90 * t / 2000
}

fn bpm_text(b: f64) -> String {
    if (b - b.round()).abs() < 0.05 { format!("{:.0}", b) } else { format!("{:.1}", b) }
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
const SIDE_W: f32 = 220.0;
const RIGHT_W: f32 = 340.0;
/// left edge of the main column (right of the song list)
const X0: f32 = M + SIDE_W + M;
const SIDE_CARD: R = R::new(M, 72.0, SIDE_W, H - 72.0 - M);
const ADD_FILE_BTN: R = R::new(SIDE_CARD.x + 14.0, SIDE_CARD.y + 50.0, (SIDE_W - 28.0 - 8.0) / 2.0, 30.0);
const ADD_DIR_BTN: R = R::new(ADD_FILE_BTN.x + ADD_FILE_BTN.w + 8.0, ADD_FILE_BTN.y, ADD_FILE_BTN.w, 30.0);
const LIST: R = R::new(SIDE_CARD.x + 8.0, SIDE_CARD.y + 94.0, SIDE_W - 16.0, SIDE_CARD.h - 94.0 - 10.0);
const LIST_ROW_H: f32 = 30.0;
const VIS_CARD: R = R::new(X0, 72.0, W - X0 - M * 2.0 - RIGHT_W, H - 72.0 - M - 84.0 - 12.0);
const TRANSPORT: R = R::new(X0, H - M - 84.0, W - X0 - M * 2.0 - RIGHT_W, 84.0);
const SET_H: f32 = 504.0;
const SET_CARD: R = R::new(W - M - RIGHT_W, 72.0, RIGHT_W, SET_H);
const STAT_CARD: R = R::new(W - M - RIGHT_W, 72.0 + SET_H + 12.0, RIGHT_W, H - M - (72.0 + SET_H + 12.0));
const OPEN_BTN: R = R::new(W - M - 150.0, 15.0, 150.0, 38.0);
/// the Roblox FPS row sits above the stepper rows
const FPS_ROW: f32 = SET_CARD.y + 56.0;
const ROW0: f32 = FPS_ROW + 42.0;
const ROW_H: f32 = 42.0;
const TOG0: f32 = ROW0 + ROW_H * 7.0 + 10.0;
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
    (R::new(right - 140.0, y, 30.0, 30.0), R::new(right - 30.0, y, 30.0, 30.0))
}
/// BPM / 배율 switch next to the "템포" label (single-tempo songs only).
fn speed_unit_rect() -> R {
    R::new(SET_CARD.x + 58.0, ROW0 + 6.0, 78.0, 20.0)
}
/// Segment `i` of the Roblox FPS choice.
fn fps_seg(i: usize) -> R {
    let (_, b) = stepper_rects(0);
    let x0 = SET_CARD.x + 168.0;
    let w = (b.x + b.w - x0) / FPS_CHOICES.len() as f32;
    R::new(x0 + w * i as f32, FPS_ROW + 6.0, w, 30.0)
}
fn field_rect(row: usize) -> R {
    let (a, b) = stepper_rects(row);
    R::new(a.x + a.w + 4.0, a.y, b.x - a.x - a.w - 8.0, a.h)
}
fn list_rows() -> usize {
    (LIST.h / LIST_ROW_H) as usize
}
/// Rect of sidebar row `i` given the scroll, if it is visible.
fn song_rect(m: &Model, i: usize) -> Option<R> {
    let k = i.checked_sub(m.song_scroll).filter(|&k| k < list_rows())?;
    Some(R::new(LIST.x, LIST.y + k as f32 * LIST_ROW_H, LIST.w, LIST_ROW_H))
}
fn song_del_rect(r: &R) -> R {
    R::new(r.x + r.w - 28.0, r.y + 4.0, 22.0, 22.0)
}
fn toggle_row(i: usize) -> R {
    R::new(SET_CARD.x + 8.0, TOG0 + i as f32 * TOG_H, SET_CARD.w - 16.0, TOG_H)
}

/// Every clickable thing and where it is.
pub fn widgets(m: &Model) -> Vec<(R, Wid)> {
    let mut v = vec![(OPEN_BTN, Wid::Open), (ADD_FILE_BTN, Wid::AddFile), (ADD_DIR_BTN, Wid::AddFolder)];
    for (i, row) in m.songs.iter().enumerate() {
        let Some(r) = song_rect(m, i) else { continue };
        if row.kind != RowKind::InFolder {
            v.push((song_del_rect(&r), Wid::SongDel(i)));
        }
        if row.file.is_some() {
            v.push((r, Wid::Song(i)));
        }
    }
    let (px, py, pr) = play_btn();
    v.push((R::new(px - pr, py - pr, pr * 2.0, pr * 2.0), Wid::Play));
    let (sx, sy, sr) = stop_btn();
    v.push((R::new(sx - sr, sy - sr, sr * 2.0, sr * 2.0), Wid::Stop));
    let sb = seek_bar();
    v.push((R::new(sb.x - 6.0, sb.y - 10.0, sb.w + 12.0, sb.h + 20.0), Wid::Seek));
    let pairs = [
        (Wid::SpeedDn, Wid::SpeedUp),
        (Wid::TransDn, Wid::TransUp),
        (Wid::TolDn, Wid::TolUp),
        (Wid::RateDn, Wid::RateUp),
        (Wid::WinDn, Wid::WinUp),
        (Wid::HumanDn, Wid::HumanUp),
        (Wid::CountDn, Wid::CountUp),
    ];
    for (i, (dn, up)) in pairs.iter().enumerate() {
        let (a, b) = stepper_rects(i);
        v.push((a, *dn));
        v.push((field_rect(i), Wid::Field(i)));
        v.push((b, *up));
    }
    if m.base_bpm().is_some() {
        v.push((speed_unit_rect(), Wid::SpeedUnit));
    }
    for i in 0..FPS_CHOICES.len() {
        v.push((fps_seg(i), Wid::Fps(i as u8)));
    }
    for (i, w) in [Wid::Pedal, Wid::Roblox, Wid::Vis].iter().enumerate() {
        v.push((toggle_row(i), *w));
    }
    v
}

/// The -/+ buttons of the settings rows.
pub fn is_stepper(w: Wid) -> bool {
    use Wid::*;
    matches!(w, SpeedDn | SpeedUp | TransDn | TransUp | TolDn | TolUp | RateDn | RateUp | WinDn | WinUp | HumanDn | HumanUp | CountDn | CountUp)
}

pub fn in_song_list(x: f32, y: f32) -> bool {
    SIDE_CARD.contains(x, y)
}

pub fn hit(m: &Model, x: f32, y: f32) -> Option<Wid> {
    widgets(m).into_iter().find(|(r, _)| r.contains(x, y)).map(|(_, w)| w)
}

/// Seek position (0..1) for a mouse x over the seek bar.
pub fn seek_fraction(x: f32) -> f64 {
    let sb = seek_bar();
    (((x - sb.x) / sb.w) as f64).clamp(0.0, 1.0)
}

/// The part of the window that changes while a song plays (visualizer and
/// transport). Everything else comes from the static layer.
pub fn dynamic_rect() -> R {
    R::new(VIS_CARD.x, VIS_CARD.y, VIS_CARD.w.max(TRANSPORT.w), TRANSPORT.y + TRANSPORT.h - VIS_CARD.y)
}

/// Seek bar fill as a fraction of the song.
fn progress(m: &Model) -> f32 {
    let len = m.length_us();
    (if len > 0 { (m.pos_us as f64 / len as f64).clamp(0.0, 1.0) } else { 0.0 }) as f32
}

/// Changes exactly when the moving part of the frame would look different:
/// every position while falling notes are shown, otherwise only when the
/// clock or the seek bar (in quarter device pixels) moves, or the input-blocked
/// warning appears.
pub fn frame_key(m: &Model, scale: f32) -> i64 {
    let shown = if m.vis_on && !m.vis.is_empty() {
        m.pos_us
    } else {
        let bar = (seek_bar().w * progress(m) * scale * 4.0).round() as i64;
        (m.pos_us.max(0) / 1_000_000) << 24 | bar
    };
    let count = if m.state == ST_COUNT { count_digit(m) } else { 0 };
    (shown << 1 | m.blocked as i64) << 4 | count
}

/// Whole seconds shown while counting down (3, 2, 1).
fn count_digit(m: &Model) -> i64 {
    (m.count_left_us.max(0) + 999_999) / 1_000_000
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
fn static_sig(scale: f32, m: &Model) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (m.fps_idx, m.speed.to_bits(), m.transpose, m.tol_ms, m.rate, m.rate_win_ms, m.human).hash(&mut h);
    (m.pedal, m.speed_bpm, m.roblox_only, m.vis_on, &m.file_name, m.analyzing, &m.error).hash(&mut h);
    (m.state, m.hover, m.pressed, scale.to_bits()).hash(&mut h);
    m.edit.as_ref().map(|e| (e.row, &e.text, e.fresh)).hash(&mut h);
    (m.songs_ver, m.song_sel, m.song_scroll).hash(&mut h);
    if let Some(su) = &m.summary {
        (su.kept, su.dropped, su.tempo_changes, su.max_late_us, su.ok, su.bpm.map(f64::to_bits)).hash(&mut h);
    }
    h.finish()
}

pub fn render(cv: &mut Canvas, m: &Model) {
    let sig = static_sig(cv.s, m);
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
    song_list(cv, m);
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

fn small_btn(cv: &mut Canvas, r: R, hov: bool, label: &str) {
    cv.rr(r.x, r.y, r.w, r.h, 9.0, col(if hov { 0x283044 } else { CARD2 }, 1.0));
    cv.rr_stroke(r.x + 0.5, r.y + 0.5, r.w - 1.0, r.h - 1.0, 9.0, col(BORDER, 1.0), 1.0);
    cv.text_c(r.cx(), r.cy() + 1.0, 12.5, true, col(TEXT, 0.92), label);
}

fn folder_icon(cv: &mut Canvas, x: f32, y: f32, c: Color) {
    cv.rr(x, y, 6.0, 3.0, 1.0, c);
    cv.rr(x, y + 2.0, 13.0, 9.0, 2.0, c);
}

fn song_list(cv: &mut Canvas, m: &Model) {
    let c = SIDE_CARD;
    card(cv, c);
    cv.text(c.x + 18.0, c.y + 18.0, 15.0, true, col(TEXT, 1.0), "곡 목록");
    let n = m.songs.iter().filter(|r| r.file.is_some()).count();
    if n > 0 {
        cv.text_r(c.x + c.w - 18.0, c.y + 21.0, 11.5, false, col(DIM, 1.0), &format!("{}곡", thousands(n)));
    }
    small_btn(cv, ADD_FILE_BTN, m.hover == Some(Wid::AddFile), "+ 파일");
    small_btn(cv, ADD_DIR_BTN, m.hover == Some(Wid::AddFolder), "+ 폴더");

    if m.songs.is_empty() {
        let y = LIST.y + 40.0;
        cv.text_c(LIST.cx(), y, 12.5, false, col(SUB, 1.0), "목록이 비어 있습니다");
        cv.text_c(LIST.cx(), y + 22.0, 11.5, false, col(DIM, 1.0), "파일이나 폴더를 창에");
        cv.text_c(LIST.cx(), y + 40.0, 11.5, false, col(DIM, 1.0), "끌어다 놓아도 됩니다");
        return;
    }
    for (i, row) in m.songs.iter().enumerate() {
        let Some(r) = song_rect(m, i) else { continue };
        let hov = matches!(m.hover, Some(Wid::Song(k) | Wid::SongDel(k)) if k == i);
        let sel = m.song_sel == Some(i);
        if sel {
            cv.rr(r.x, r.y + 1.0, r.w, r.h - 2.0, 8.0, col(ACCENT, 0.22));
            cv.rr(r.x, r.y + 7.0, 3.0, r.h - 14.0, 1.5, col(ACCENT, 1.0));
        } else if hov && row.file.is_some() {
            cv.rr(r.x, r.y + 1.0, r.w, r.h - 2.0, 8.0, col(CARD2, 1.0));
        }
        let del = hov && row.kind != RowKind::InFolder;
        let right = r.x + r.w - if del { 32.0 } else { 10.0 };
        let ty = r.y + 7.0;
        match row.kind {
            RowKind::Folder => {
                folder_icon(cv, r.x + 10.0, r.y + 10.0, col(AMBER, 0.85));
                let x = r.x + 30.0;
                let t = ellipsize(cv, &row.label, 12.5, right - x);
                cv.text(x, ty, 12.5, false, col(SUB, 1.0), &t);
            }
            RowKind::File | RowKind::InFolder => {
                let x = r.x + if row.kind == RowKind::InFolder { 30.0 } else { 14.0 };
                let dot = if sel { ACCENT } else { DIM };
                cv.circle(x + 2.5, r.cy(), 2.5, &solid(col(dot, 1.0)));
                let tx = x + 12.0;
                let t = ellipsize(cv, &row.label, 12.5, right - tx);
                cv.text(tx, ty, 12.5, false, col(if sel { TEXT } else { 0xC5CCD9 }, 1.0), &t);
            }
        }
        if del {
            let d = song_del_rect(&r);
            let on = m.hover == Some(Wid::SongDel(i));
            if on {
                cv.rr(d.x, d.y, d.w, d.h, 6.0, col(RED, 0.18));
            }
            cv.text_c(d.cx(), d.cy() + 0.5, 14.0, false, col(if on { RED } else { SUB }, 1.0), "×");
        }
    }
    // scroll bar
    let vis = list_rows();
    if m.songs.len() > vis {
        let track = LIST.h - 8.0;
        let h = (track * vis as f32 / m.songs.len() as f32).max(24.0);
        let max = (m.songs.len() - vis) as f32;
        let y = LIST.y + 4.0 + (track - h) * m.song_scroll as f32 / max;
        cv.rr(c.x + c.w - 6.0, y, 3.0, h, 1.5, col(0xFFFFFF, 0.14));
    }
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
    /// legend strip above the lanes (static, notes never reach it)
    legend: R,
    area: R,
    kb: R,
}
const LANE_BG: u32 = 0x0C1019;
fn vis_geo() -> VisGeo {
    let c = VIS_CARD;
    let inner = R::new(c.x + 12.0, c.y + 12.0, c.w - 24.0, c.h - 24.0);
    let kb_h = 92.0;
    let leg_h = 24.0;
    let top = inner.y + leg_h + 6.0;
    VisGeo {
        legend: R::new(inner.x, inner.y, inner.w, leg_h),
        kb: R::new(inner.x, inner.y + inner.h - kb_h, inner.w, kb_h),
        area: R::new(inner.x, top, inner.w, inner.y + inner.h - kb_h - 4.0 - top),
    }
}

// Keys are plain rectangles so a lit key is one flat fill over the cached one.
fn white_key(cv: &mut Canvas, p: u8, kb: &R, c: Color) {
    let (x, w) = key_x(p, kb);
    cv.rect(x + 0.5, kb.y, w - 1.0, kb.h, c);
}
fn black_key(cv: &mut Canvas, p: u8, kb: &R, c: Color) {
    let (x, w) = key_x(p, kb);
    cv.rect(x, kb.y - 1.0, w, kb.h * 0.62, c);
}

fn visualizer_static(cv: &mut Canvas, _m: &Model) {
    card(cv, VIS_CARD);
    let VisGeo { legend, area, kb } = vis_geo();
    let mut lx = legend.x + 4.0;
    for (c, t, a) in [(NOTE_WHITE, "흰건반", 1.0), (NOTE_SHIFT, "Shift", 1.0), (NOTE_CTRL, "Ctrl", 1.0), (0x8A96B4, "뺀 음", 0.35)] {
        cv.rr(lx, legend.cy() - 5.0, 10.0, 10.0, 3.0, col(c, a));
        lx += 16.0 + cv.text(lx + 16.0, legend.cy() - 8.5, 11.5, false, col(SUB, 1.0), t) + 14.0;
    }
    cv.rr(area.x, area.y, area.w, area.h + 4.0, 10.0, col(LANE_BG, 1.0));
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
    let VisGeo { area, kb, .. } = vis_geo();
    for p in (24..=108).step_by(12) {
        let (x, _) = key_x(p, &kb);
        cv.rect(x, area.y + 6.0, 1.0, area.h - 6.0, col(0xFFFFFF, 0.045));
    }
    let now = m.pos_us;
    let pps = area.h as f64 / LOOKAHEAD_US;
    let hit_y = area.y + area.h;
    let mut lit: [Option<u32>; 128] = [None; 128];

    if m.vis_on && !m.vis.is_empty() {
        let v = &m.vis;
        let lo = v.partition_point(|n| n.start < now - m.vis_maxdur);
        let hi = v.partition_point(|n| (n.start as f64) < now as f64 + LOOKAHEAD_US);
        let visible = &v[lo..hi];
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
                // flat fill; the 1px gap at the top keeps repeated notes apart
                let h = (yb - yt).max(4.0);
                cv.rect(x + 1.5, yt + 1.0, w - 3.0, h - 1.0, col(nc, 1.0));
            }
        }
    }

    // hit line
    cv.rect(area.x, hit_y - 1.5, area.w, 2.0, col(ACCENT, 1.0));

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
    if m.state == ST_COUNT {
        cv.rect(area.x, area.y, area.w, area.h, col(BG, 0.6));
        cv.circle(area.cx(), area.cy() - 10.0, 64.0, &solid(col(CARD2, 0.95)));
        cv.text_c(area.cx(), area.cy() - 8.0, 64.0, true, col(TEXT, 1.0), &count_digit(m).to_string());
        cv.text_c(area.cx(), area.cy() + 82.0, 13.5, false, col(SUB, 1.0), "곧 시작합니다 · F6 취소");
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
    if matches!(m.state, ST_PLAYING | ST_ARMED | ST_COUNT) {
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
    let frac = progress(m);
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
            ST_ARMED if m.countdown > 0 => (format!("Roblox 창을 클릭하면 {}초 뒤 시작합니다", m.countdown), AMBER),
            ST_ARMED => ("Roblox 창을 클릭하면 시작합니다".into(), AMBER),
            ST_COUNT => (format!("{}초 뒤 시작", count_digit(m)), AMBER),
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

    // Roblox FPS: segmented choice
    cv.text(c.x + 20.0, FPS_ROW + 8.0, 13.5, false, col(TEXT, la), "Roblox FPS");
    cv.text(c.x + 20.0, FPS_ROW + 27.0, 10.5, false, col(DIM, la), "게임의 최대 프레임과 같게");
    {
        let (f, l) = (fps_seg(0), fps_seg(FPS_CHOICES.len() - 1));
        cv.rr(f.x, f.y, l.x + l.w - f.x, f.h, 9.0, col(CARD2, 1.0));
    }
    for (i, v) in FPS_CHOICES.iter().enumerate() {
        let r = fps_seg(i);
        let sel = m.fps_idx == i;
        if sel {
            cv.rr(r.x + 2.0, r.y + 2.0, r.w - 4.0, r.h - 4.0, 7.0, col(ACCENT, la));
        } else if m.hover == Some(Wid::Fps(i as u8)) && ed {
            cv.rr(r.x + 2.0, r.y + 2.0, r.w - 4.0, r.h - 4.0, 7.0, col(0x283044, 1.0));
        }
        let a = if sel { la } else { 0.7 * la };
        cv.text_c(r.cx(), r.cy() + 1.0, 12.5, sel, col(TEXT, a), &v.to_string());
    }

    let (speed_label, speed_sub) = match m.base_bpm() {
        Some(b) => ("템포", format!("원곡 {} BPM", bpm_text(b))),
        None => ("재생 속도", String::new()),
    };
    let rows: [(&str, &str, String, Wid, Wid); 7] = [
        (speed_label, &speed_sub, m.speed_text(), Wid::SpeedDn, Wid::SpeedUp),
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
            "최대 타건",
            "Roblox 렉 방지 · 0 = 무제한",
            if m.rate == 0 { "무제한".into() } else { format!("{}회", m.rate) },
            Wid::RateDn,
            Wid::RateUp,
        ),
        (
            "제한 구간",
            "이 시간 안에 최대 타건까지",
            format!("{} ms", m.rate_win_ms),
            Wid::WinDn,
            Wid::WinUp,
        ),
        (
            "사람처럼 연주",
            "타이밍 오차 · 화음 엇갈림",
            crate::humanize::LEVEL_NAMES[m.human as usize].into(),
            Wid::HumanDn,
            Wid::HumanUp,
        ),
        (
            "카운트다운",
            "재생 전 대기 · 0 = 끔",
            if m.countdown == 0 { "끔".into() } else { format!("{}초", m.countdown) },
            Wid::CountDn,
            Wid::CountUp,
        ),
    ];
    if m.base_bpm().is_some() {
        let r = speed_unit_rect();
        let hov = m.hover == Some(Wid::SpeedUnit) && ed;
        cv.rr(r.x, r.y, r.w, r.h, 10.0, col(if hov { 0x283044 } else { CARD2 }, 1.0));
        let half = r.w / 2.0;
        let on_x = if m.speed_bpm { r.x } else { r.x + half };
        cv.rr(on_x + 2.0, r.y + 2.0, half - 4.0, r.h - 4.0, 8.0, col(ACCENT, la));
        for (k, t) in ["BPM", "배율"].iter().enumerate() {
            let sel = (k == 0) == m.speed_bpm;
            let a = if sel { la } else { 0.55 * la };
            cv.text_c(r.x + half * (k as f32 + 0.5), r.cy() + 1.0, 11.0, sel, col(TEXT, a), t);
        }
    }
    for (i, (t, sub, val, dn, up)) in rows.iter().enumerate() {
        label(cv, i, t, sub);
        let (a, b) = stepper_rects(i);
        for (r, w, minus) in [(a, *dn, true), (b, *up, false)] {
            let hov = m.hover == Some(w) && ed;
            let bg = if m.pressed == Some(w) && ed { 0x343E58 } else if hov { 0x283044 } else { CARD2 };
            cv.rr(r.x, r.y, r.w, r.h, 9.0, col(bg, 1.0));
            cv.rr(r.cx() - 5.5, r.cy() - 0.9, 11.0, 1.8, 0.9, col(TEXT, la));
            if !minus {
                cv.rr(r.cx() - 0.9, r.cy() - 5.5, 1.8, 11.0, 0.9, col(TEXT, la));
            }
        }
        let f = field_rect(i);
        let edit = m.edit.as_ref().filter(|e| e.row == i);
        let line = match edit {
            Some(_) => col(ACCENT, 1.0),
            None if m.hover == Some(Wid::Field(i)) && ed => col(0x3A4560, 1.0),
            None => col(BORDER, 1.0),
        };
        cv.rr(f.x, f.y, f.w, f.h, 7.0, col(BG, 1.0));
        cv.rr_stroke(f.x + 0.5, f.y + 0.5, f.w - 1.0, f.h - 1.0, 6.5, line, 1.0);
        match edit {
            Some(e) => {
                let tw = cv.measure(13.5, true, &e.text);
                let tx = f.cx() - tw / 2.0;
                if e.fresh && !e.text.is_empty() {
                    cv.rr(tx - 2.0, f.y + 6.0, tw + 4.0, f.h - 12.0, 3.0, col(ACCENT, 0.45));
                }
                cv.text_c(f.cx(), f.cy() + 1.0, 13.5, true, col(TEXT, 1.0), &e.text);
                if !e.fresh {
                    cv.rect(tx + tw + 1.0, f.y + 7.0, 1.5, f.h - 14.0, col(TEXT, 1.0));
                }
            }
            None => cv.text_c(f.cx(), f.cy() + 1.0, 13.5, true, col(TEXT, la), val),
        }
    }

    // divider + toggles
    cv.rect(c.x + 20.0, TOG0 - 6.0, c.w - 40.0, 1.0, col(BORDER, 1.0));
    let toggles = [
        ("서스테인 페달 → Space", m.pedal, Wid::Pedal, ed),
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
    let y = c.y + 44.0;
    let w = cv.text(c.x + 20.0, y, 30.0, true, col(TEXT, 1.0), &format!("{pct:.1}"));
    cv.text(c.x + 20.0 + w + 2.0, y + 13.0, 15.0, true, col(SUB, 1.0), "%");
    cv.text(c.x + 20.0 + w + 26.0, y + 15.0, 12.0, false, col(SUB, 1.0), "원래 자리에서 연주되는 음");
    // ratio bar
    let by = y + 42.0;
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
        cv.text(x, by + 16.0, 11.0, false, col(DIM, 1.0), k);
        cv.text(x, by + 32.0, 14.5, true, col(TEXT, 1.0), v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Row;
    use crate::schedule::VisNote;

    fn playing(vis: bool) -> Model {
        let mut m = Model::default();
        m.state = ST_PLAYING;
        m.vis_on = vis;
        m.set_vis(vec![VisNote { start: 0, end: 180_000_000, pitch: 60, kept: true }]);
        m.summary = Some(Summary { length_us: 180_000_000, ok: true, ..Summary::default() });
        m
    }

    #[test]
    fn frame_key_without_visualizer_follows_shown_time_and_bar() {
        let at = |us: i64| frame_key(&Model { pos_us: us, ..playing(false) }, 1.0);
        // 3-minute song: a few ms moves neither the clock nor the bar
        assert_eq!(at(10_000_000), at(10_005_000));
        // the clock ticks over
        assert_ne!(at(10_990_000), at(11_000_000));
        // the bar moves by a visible amount
        assert_ne!(at(10_000_000), at(10_900_000));
    }

    #[test]
    fn frame_key_with_visualizer_changes_every_frame() {
        let at = |us: i64| frame_key(&Model { pos_us: us, ..playing(true) }, 1.0);
        assert_ne!(at(10_000_000), at(10_016_000));
    }

    #[test]
    fn frame_key_shows_blocked_warning() {
        let m = playing(false);
        assert_ne!(frame_key(&m, 1.0), frame_key(&Model { blocked: true, ..playing(false) }, 1.0));
    }

    #[test]
    fn frame_key_follows_countdown_seconds() {
        let at = |left: i64| frame_key(&Model { state: ST_COUNT, count_left_us: left, ..playing(false) }, 1.0);
        assert_eq!(at(2_400_000), at(2_100_000));
        assert_ne!(at(2_001_000), at(2_000_000)); // 3 -> 2
    }

    #[test]
    fn countdown_is_a_playback_setting_not_an_analysis_one() {
        let mut m = Model::default();
        assert_eq!(m.countdown, 3);
        // never triggers re-analysis
        assert!(!m.click_setting(Wid::CountUp));
        assert_eq!(m.countdown, 4);
        for _ in 0..20 {
            m.click_setting(Wid::CountDn);
        }
        assert_eq!(m.countdown, 0);
        m.begin_edit(6);
        m.edit_char('9');
        m.edit_char('9');
        assert!(!m.commit_edit());
        assert_eq!(m.countdown, 10);
    }

    #[test]
    fn single_tempo_song_steps_speed_by_whole_bpm() {
        let mut m = Model::default();
        m.summary = Some(Summary { bpm: Some(120.0), ..Summary::default() });
        assert_eq!(m.speed_text(), "120 BPM");
        assert!(m.click_setting(Wid::SpeedUp));
        assert!((m.speed - 121.0 / 120.0).abs() < 1e-12);
        assert_eq!(m.speed_text(), "121 BPM");
        m.click_setting(Wid::SpeedDn);
        m.click_setting(Wid::SpeedDn);
        assert_eq!(m.speed_text(), "119 BPM");
        // multi-tempo songs keep the plain multiplier
        m.summary = Some(Summary { bpm: None, ..Summary::default() });
        m.speed = 1.0;
        m.click_setting(Wid::SpeedUp);
        assert_eq!(m.speed, 1.05);
        assert_eq!(m.speed_text(), "1.05x");
    }

    #[test]
    fn opening_another_song_resets_speed() {
        let mut m = Model::default();
        m.speed = 1.1;
        m.open_song("b.mid".into());
        assert_eq!(m.speed, 1.0);
        assert_eq!(m.file_name.as_deref(), Some("b.mid"));
    }

    fn rows(n: usize) -> Vec<Row> {
        let mut v = vec![Row { label: "dir".into(), file: None, entry: 0, kind: RowKind::Folder }];
        for i in 1..n {
            v.push(Row { label: format!("s{i}"), file: Some(format!("s{i}.mid").into()), entry: 0, kind: RowKind::InFolder });
        }
        v
    }

    #[test]
    fn song_rows_hit_by_scroll_position() {
        let mut m = Model::default();
        m.set_songs(rows(40));
        let x = LIST.x + 40.0;
        // folder header: not playable, only its remove button is clickable
        assert_eq!(hit(&m, x, LIST.y + 5.0), None);
        let d = song_del_rect(&song_rect(&m, 0).unwrap());
        assert_eq!(hit(&m, d.cx(), d.cy()), Some(Wid::SongDel(0)));
        assert_eq!(hit(&m, x, LIST.y + LIST_ROW_H * 2.0 + 5.0), Some(Wid::Song(2)));
        // rows inside a folder have no remove button
        let d = song_del_rect(&song_rect(&m, 2).unwrap());
        assert_eq!(hit(&m, d.cx(), d.cy()), Some(Wid::Song(2)));
        m.scroll_songs(3);
        assert_eq!(hit(&m, x, LIST.y + 5.0), Some(Wid::Song(3)));
    }

    #[test]
    fn song_scroll_is_clamped() {
        let mut m = Model::default();
        m.set_songs(rows(40));
        m.scroll_songs(-5);
        assert_eq!(m.song_scroll, 0);
        m.scroll_songs(1000);
        assert_eq!(m.song_scroll, 40 - list_rows());
        // a shorter list pulls the scroll back
        m.set_songs(rows(3));
        assert_eq!(m.song_scroll, 0);
    }

    #[test]
    fn hold_repeat_waits_then_speeds_up() {
        let at: Vec<u64> = std::iter::successors(Some(0), |&t| Some(next_repeat(t))).take(40).collect();
        assert_eq!(at[1], 400);
        let gaps: Vec<u64> = at.windows(2).skip(1).map(|w| w[1] - w[0]).collect();
        assert!(gaps.windows(2).all(|g| g[1] <= g[0]), "{gaps:?}");
        assert!(gaps[0] >= 100 && *gaps.last().unwrap() <= 30, "{gaps:?}");
    }

    fn typed(m: &mut Model, row: usize, s: &str) -> bool {
        m.begin_edit(row);
        s.chars().for_each(|c| m.edit_char(c));
        m.commit_edit()
    }

    #[test]
    fn edit_opens_with_value_selected_and_typing_replaces_it() {
        let mut m = Model::default();
        m.begin_edit(3);
        let e = m.edit.as_ref().unwrap();
        assert_eq!((e.text.as_str(), e.fresh), ("20", true));
        m.edit_char('5');
        assert_eq!(m.edit.as_ref().unwrap().text, "5");
        m.edit_backspace();
        m.edit_char('4');
        m.edit_char('0');
        assert!(m.commit_edit());
        assert_eq!(m.rate, 40);
        assert!(m.edit.is_none());
    }

    #[test]
    fn typed_values_are_clamped_to_each_range() {
        let mut m = Model::default();
        assert!(typed(&mut m, 1, "-30"));
        assert_eq!(m.transpose, -24);
        typed(&mut m, 4, "5000");
        assert_eq!(m.rate_win_ms, 1000);
        typed(&mut m, 2, "999");
        assert_eq!(m.tol_ms, Some(150));
        typed(&mut m, 5, "7");
        assert_eq!(m.human, 3);
        typed(&mut m, 0, "1.5");
        assert_eq!(m.speed, 1.5);
        typed(&mut m, 0, "9");
        assert_eq!(m.speed, 3.0);
    }

    #[test]
    fn typed_tempo_is_in_bpm_and_empty_tolerance_is_automatic() {
        let mut m = Model::default();
        m.summary = Some(Summary { bpm: Some(120.0), ..Summary::default() });
        assert!(typed(&mut m, 0, "150"));
        assert_eq!(m.speed_text(), "150 BPM");
        m.tol_ms = Some(30);
        m.begin_edit(2);
        m.edit_backspace();
        assert!(m.commit_edit());
        assert_eq!(m.tol_ms, None);
    }

    #[test]
    fn bad_or_unchanged_input_changes_nothing() {
        let mut m = Model::default();
        assert!(!typed(&mut m, 3, "abc"));
        assert!(!typed(&mut m, 3, "-"));
        assert!(!typed(&mut m, 3, "20"));
        assert_eq!(m.rate, 20);
        m.begin_edit(3);
        m.edit_char('9');
        m.cancel_edit();
        assert_eq!(m.rate, 20);
        assert!(m.edit.is_none());
    }

    #[test]
    fn no_edit_while_playing() {
        let mut m = playing(false);
        m.begin_edit(3);
        assert!(m.edit.is_none());
    }

    #[test]
    fn single_tempo_song_can_step_speed_as_multiplier() {
        let mut m = Model::default();
        m.summary = Some(Summary { bpm: Some(120.0), ..Summary::default() });
        assert!(!m.click_setting(Wid::SpeedUnit));
        assert_eq!(m.speed_text(), "1.00x");
        m.click_setting(Wid::SpeedUp);
        assert_eq!(m.speed_text(), "1.05x");
        assert!(typed(&mut m, 0, "1.5"));
        assert_eq!(m.speed, 1.5);
        m.click_setting(Wid::SpeedUnit);
        assert_eq!(m.speed_text(), "180 BPM");
    }

    #[test]
    fn unit_switch_only_for_single_tempo_songs() {
        let mut m = Model::default();
        assert!(!widgets(&m).iter().any(|(_, w)| *w == Wid::SpeedUnit));
        m.summary = Some(Summary { bpm: Some(120.0), ..Summary::default() });
        assert!(widgets(&m).iter().any(|(_, w)| *w == Wid::SpeedUnit));
    }

    #[test]
    fn typing_and_holding_repaint_the_settings_card() {
        let mut m = Model::default();
        let sig = |m: &Model| static_sig(1.0, m);
        let s0 = sig(&m);
        m.pressed = Some(Wid::RateUp);
        assert_ne!(sig(&m), s0);
        m.pressed = None;
        m.begin_edit(3);
        let s1 = sig(&m);
        assert_ne!(s1, s0);
        m.edit_char('5');
        assert_ne!(sig(&m), s1);
    }
}
