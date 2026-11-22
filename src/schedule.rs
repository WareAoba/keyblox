//! Turns ideal note/pedal events into a key-press timeline for a Roblox
//! virtual piano, keeping every played note at its original place in time.
//!
//! The one hard limit: Roblox reads Shift/Ctrl when it *processes* a key,
//! once per frame. So after a key-down made with modifier state X, the
//! state may only change once a frame has surely passed (DWELL); otherwise
//! the note sounds as the wrong pitch (e.g. `T` as `t`).
//!
//! Notes are never pushed back. Each note is struck at its own time (or at
//! most `tolerance` later, enough to roll a mixed white/black chord); a note
//! that cannot fit is removed. Which notes to keep is decided over the whole
//! song at once (dynamic programming) to keep as many notes as possible in
//! place. Key presses themselves are not throttled (HOLD / GAP default ~0).

use crate::layout::{key_for, Mod, SC_LCTRL, SC_LSHIFT, SC_SPACE};
use crate::midi::{Ev, Kind};
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// modifier state must stay unchanged this long after a key-down under it
    pub dwell: i64,
    /// minimum time a key is held (0 = as short as the MIDI says)
    pub hold: i64,
    /// minimum up-time before the same key is pressed again
    pub gap: i64,
    /// how late a note may be struck at most; anything later is removed
    pub tolerance: i64,
    /// at most this many key strikes in any `rate_window` (0 = unlimited);
    /// keeps Roblox from choking
    pub rate_count: usize,
    pub rate_window: i64,
}

/// Default tolerance: enough to roll a chord that needs all three modifier
/// states (plain + Shift + Ctrl = two switches), plus a little margin.
pub fn auto_tolerance(dwell: i64) -> i64 {
    dwell * 2 + 2_000
}

/// The player may send events up to this much early to batch them (see player.rs);
/// the dwell default includes it so the modifier rule still holds.
pub const COALESCE_US: i64 = 1_000;

impl Params {
    pub fn from_fps(fps: f64) -> Self {
        // 1.25 frames: guarantees a frame boundary with margin for frame jitter
        let f = (1_000_000.0 / fps * 1.25).round() as i64;
        Params { dwell: f + COALESCE_US, hold: 0, gap: 0, tolerance: auto_tolerance(f + COALESCE_US), rate_count: 20, rate_window: 100_000 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Down(u16),
    Up(u16),
}

#[derive(Clone, Copy, Debug)]
pub struct Action {
    pub t: i64,
    pub act: Act,
    /// for note key-downs: pitch and required modifier (used by the checker)
    pub note: Option<(u8, Mod)>,
}

/// One note for the visualizer: actually struck (kept) or removed.
#[derive(Clone, Copy, Debug)]
pub struct VisNote {
    pub pitch: u8,
    pub start: i64,
    pub end: i64,
    pub kept: bool,
}

#[derive(Default)]
pub struct Schedule {
    pub actions: Vec<Action>,
    /// sorted by start
    pub vis: Vec<VisNote>,
    pub notes: usize,        // note-ons in the input
    pub strikes: usize,      // key strikes in the output
    pub merged: usize,       // same pitch at the same moment (layered tracks) -> one strike
    pub dropped: usize,      // removed: could not be struck in place
    pub delayed: usize,      // strikes later than ideal by > 1 ms (chord roll)
    pub max_late: i64,       // us
    pub sum_late: i64,       // us
    pub cut_by_clash: usize, // held note released early because another note needs its key
}

const NEG: i64 = i64::MIN / 4;

#[derive(Clone, Copy)]
struct Note {
    t: i64,
    off: i64,
    pitch: u8,
    m: Mod,
}

fn mod_scan(m: Mod) -> Option<u16> {
    match m {
        Mod::None => None,
        Mod::Shift => Some(SC_LSHIFT),
        Mod::Ctrl => Some(SC_LCTRL),
    }
}

fn mod_idx(m: Mod) -> usize {
    m as usize
}

/// Pair note-ons with note-offs (FIFO per pitch) and merge identical
/// (same pitch, same instant) notes from layered tracks.
fn collect_notes(events: &[Ev], end: i64) -> (Vec<Note>, usize, usize) {
    let mut notes: Vec<Note> = vec![];
    let mut open: Vec<std::collections::VecDeque<usize>> = vec![Default::default(); 128];
    let mut n_on = 0;
    for e in events {
        match e.kind {
            Kind::NoteOn(p) => {
                n_on += 1;
                notes.push(Note { t: e.t, off: i64::MAX, pitch: p, m: key_for(p).unwrap().modifier });
                open[p as usize].push_back(notes.len() - 1);
            }
            Kind::NoteOff(p) => {
                if let Some(i) = open[p as usize].pop_front() {
                    notes[i].off = e.t;
                }
            }
            Kind::Pedal(_) => {}
        }
    }
    for n in notes.iter_mut() {
        if n.off == i64::MAX {
            n.off = end;
        }
    }
    // merge same pitch at the same instant
    notes.sort_by_key(|n| (n.t, n.pitch));
    let mut out: Vec<Note> = Vec::with_capacity(notes.len());
    let mut merged = 0;
    for n in notes {
        if let Some(last) = out.last_mut() {
            if last.t == n.t && last.pitch == n.pitch {
                last.off = last.off.max(n.off);
                merged += 1;
                continue;
            }
        }
        out.push(n);
    }
    // Within one instant, group notes by modifier state so a chord needs at most
    // one switch per state. The state the previous instant ended in goes first
    // (no switch needed), then None, Shift, Ctrl.
    let mut i = 0;
    let mut prev_last: Option<Mod> = None;
    while i < out.len() {
        let mut j = i;
        while j < out.len() && out[j].t == out[i].t {
            j += 1;
        }
        let pl = prev_last;
        out[i..j].sort_by_key(|n| (if Some(n.m) == pl { 0 } else { 1 + n.m as u8 }, n.pitch));
        prev_last = Some(out[j - 1].m);
        i = j;
    }
    (out, n_on, merged)
}

/// Choose which notes to play, and when, so that as many as possible are
/// struck in place under the modifier rule. Returns (note index, play time).
fn choose(notes: &[Note], p: Params) -> Vec<(usize, i64)> {
    let n = notes.len();
    if n == 0 {
        return vec![];
    }
    let tol = p.tolerance.max(0);
    // play delay is quantized to the nearest of NB buckets for the search only;
    // the chosen path is then re-timed exactly (no error builds up along it)
    const NB: i64 = 64;
    let bucket = ((tol + NB - 1) / NB).max(1);
    let bk = |d: i64| -> usize { ((d.max(0) + bucket / 2) / bucket) as usize };
    let nb = bk(tol) + 1;

    // per-modifier lists of note indices (sorted by index and by time)
    let mut lists: [Vec<u32>; 3] = [vec![], vec![], vec![]];
    for (i, nt) in notes.iter().enumerate() {
        lists[mod_idx(nt.m)].push(i as u32);
    }
    // first note in list c with index > k and time >= tmin
    let next = |c: usize, k: usize, tmin: i64| -> Option<usize> {
        let l = &lists[c];
        let a = l.partition_point(|&i| (i as usize) <= k);
        let b = l.partition_point(|&i| notes[i as usize].t < tmin);
        l.get(a.max(b)).map(|&i| i as usize)
    };

    const SCALE: i64 = 1 << 20; // one note outweighs any amount of roll delay
    let mut val = vec![0i64; n * nb];
    let mut nxt = vec![u32::MAX; n * nb];
    // successor state of (k, play time pk) choosing class c
    let succ = |k: usize, pk: i64, c: usize| -> Option<(usize, i64)> {
        if c == mod_idx(notes[k].m) {
            let k2 = next(c, k, pk - tol)?;
            Some((k2, pk.max(notes[k2].t)))
        } else {
            let k2 = next(c, k, pk + p.dwell - tol)?;
            Some((k2, (pk + p.dwell).max(notes[k2].t)))
        }
    };
    for k in (0..n).rev() {
        for b in 0..nb {
            let pk = notes[k].t + (b as i64 * bucket).min(tol);
            let mut best = 0i64;
            let mut arg = u32::MAX;
            for c in 0..3 {
                if let Some((k2, p2)) = succ(k, pk, c) {
                    let v = val[k2 * nb + bk(p2 - notes[k2].t)];
                    if v > best {
                        best = v;
                        arg = k2 as u32;
                    }
                }
            }
            val[k * nb + b] = SCALE - (b as i64 * bucket) / 100 + best;
            nxt[k * nb + b] = arg;
        }
    }
    // start: first note of whichever class gives the best result
    let mut start = None;
    let mut best = i64::MIN;
    for c in 0..3 {
        if let Some(&i) = lists[c].first() {
            let v = val[i as usize * nb];
            if v > best {
                best = v;
                start = Some(i as usize);
            }
        }
    }
    // walk the chosen path with exact times
    let mut path = vec![];
    let mut k = start.unwrap();
    let mut pk = notes[k].t;
    loop {
        path.push((k, pk));
        let b = bk(pk - notes[k].t).min(nb - 1);
        let k2 = nxt[k * nb + b];
        if k2 == u32::MAX {
            break;
        }
        let k2 = k2 as usize;
        let p2 = if notes[k2].m == notes[k].m { pk.max(notes[k2].t) } else { (pk + p.dwell).max(notes[k2].t) };
        k = k2;
        pk = p2;
    }
    path
}

/// Keep at most `count` strikes in any `window` (sliding window; strike times
/// along the path never decrease, so a queue of recent strikes is enough).
/// A token bucket (refills `count` per `window`, never holds more than the
/// window still allows) runs alongside, so a sustained flood is thinned
/// evenly instead of in bursts at the start of each window, while a lone
/// chord of up to `count` notes still sounds full. When a moment must lose notes, its highest (melody) and
/// lowest (bass) notes are kept first, then top-down.
fn limit_rate(notes: &[Note], path: Vec<(usize, i64)>, count: usize, window: i64) -> Vec<(usize, i64)> {
    if count == 0 || window <= 0 || path.is_empty() {
        return path;
    }
    let mut recent: VecDeque<i64> = VecDeque::new();
    let per_us = count as f64 / window as f64;
    let mut tokens = count as f64;
    let mut last_t = path[0].1;
    let mut kept: Vec<(usize, i64)> = Vec::with_capacity(path.len());
    let mut i = 0;
    while i < path.len() {
        let t0 = notes[path[i].0].t; // one musical moment = same ideal time
        let mut j = i;
        while j < path.len() && notes[path[j].0].t == t0 {
            j += 1;
        }
        let now = path[i].1;
        while recent.front().is_some_and(|&x| x <= now - window) {
            recent.pop_front();
        }
        // tokens never exceed what the window still allows, so a window that
        // was filled early does not release a second burst when it slides
        tokens = (tokens + (now - last_t) as f64 * per_us).min(count.saturating_sub(recent.len()) as f64);
        last_t = now;
        let mut group: Vec<(usize, i64)> = path[i..j].to_vec();
        let take = (tokens.floor() as usize).min(group.len());
        if take < group.len() {
            let hi = group.iter().map(|x| notes[x.0].pitch).max().unwrap();
            let lo = group.iter().map(|x| notes[x.0].pitch).min().unwrap();
            group.sort_by_key(|x| {
                let p = notes[x.0].pitch;
                (if p == hi { 0 } else if p == lo { 1 } else { 2 }, std::cmp::Reverse(p))
            });
            group.truncate(take);
            group.sort_by_key(|x| x.0); // back to path order (keeps modifier grouping)
        }
        tokens -= group.len() as f64;
        recent.extend(group.iter().map(|x| x.1));
        kept.extend(group);
        i = j;
    }
    kept
}

struct Gen {
    p: Params,
    vis: Vec<VisNote>,
    vis_of: [usize; 128], // scancode -> index in vis of the note holding it
    out: Vec<(i64, u32, Action)>,
    seq: u32,
    owner: [Option<u8>; 128],
    down_t: [i64; 128],
    up_t: [i64; 128],
    refs: [u32; 128],
    s: Schedule,
}

impl Gen {
    fn emit(&mut self, t: i64, act: Act, note: Option<(u8, Mod)>) {
        self.out.push((t, self.seq, Action { t, act, note }));
        self.seq += 1;
    }
    fn release(&mut self, sc: usize, t: i64) -> i64 {
        let up = t.max(self.down_t[sc] + self.p.hold);
        if let Some(v) = self.vis.get_mut(self.vis_of[sc]) {
            v.end = up;
        }
        self.emit(up, Act::Up(sc as u16), None);
        self.owner[sc] = None;
        self.up_t[sc] = up;
        up
    }
}

pub fn build(events: &[Ev], p: Params) -> Schedule {
    let end = events.last().map(|e| e.t).unwrap_or(0);
    let (notes, n_on, merged) = collect_notes(events, end);
    let path = choose(&notes, p);
    let path = limit_rate(&notes, path, p.rate_count, p.rate_window);

    // timeline of what will actually be played
    #[derive(Clone, Copy)]
    enum K {
        Off(u8, usize),
        Pedal(bool),
        On(u8, i64, usize), // pitch, ideal time, path id
    }
    let mut tl: Vec<(i64, u8, u32, K)> = vec![]; // (time, class, seq, kind)
    for (s, &(i, pt)) in path.iter().enumerate() {
        let nt = notes[i];
        tl.push((pt, 2, s as u32, K::On(nt.pitch, nt.t, s)));
        tl.push((nt.off.max(pt), 0, s as u32, K::Off(nt.pitch, s)));
    }
    for (s, e) in events.iter().enumerate() {
        if let Kind::Pedal(on) = e.kind {
            tl.push((e.t, 1, s as u32, K::Pedal(on)));
        }
    }
    tl.sort_by_key(|x| (x.0, x.1, x.2));

    let mut g = Gen {
        p,
        vis: Vec::with_capacity(notes.len()),
        vis_of: [usize::MAX; 128],
        out: Vec::with_capacity(tl.len() * 2),
        seq: 0,
        owner: [None; 128],
        down_t: [NEG; 128],
        up_t: [NEG; 128],
        refs: [0; 128],
        s: Schedule::default(),
    };
    let mut modstate = Mod::None;
    let mut mod_since = NEG;
    let mut last_down = NEG;
    let mut pedal = false;
    let mut pedal_t = NEG;
    let mut played = vec![false; path.len()];
    let mut note_played = vec![false; notes.len()];
    let mut late_dropped = 0;

    for &(t, _, _, k) in &tl {
        match k {
            K::Off(pitch, id) => {
                if !played[id] {
                    continue;
                }
                let pi = pitch as usize;
                if g.refs[pi] == 0 {
                    continue;
                }
                g.refs[pi] -= 1;
                let sc = key_for(pitch).unwrap().scan as usize;
                if g.refs[pi] == 0 && g.owner[sc] == Some(pitch) {
                    g.release(sc, t);
                }
            }
            K::Pedal(on) => {
                if on != pedal {
                    let tt = t.max(pedal_t + p.gap);
                    g.emit(tt, if on { Act::Down(SC_SPACE) } else { Act::Up(SC_SPACE) }, None);
                    pedal = on;
                    pedal_t = tt;
                }
            }
            K::On(pitch, ideal, id) => {
                let k = key_for(pitch).unwrap();
                let sc = k.scan as usize;
                // work out when this can really be struck before emitting anything
                let mut at = t.max(mod_since);
                let switch = k.modifier != modstate;
                if switch {
                    at = at.max(last_down + p.dwell);
                    if let Some(s) = mod_scan(k.modifier) {
                        at = at.max(g.up_t[s as usize] + p.gap);
                    }
                }
                let est_down = match g.owner[sc] {
                    Some(_) => at.max((at - p.gap).max(g.down_t[sc] + p.hold) + p.gap),
                    None => at.max(g.up_t[sc] + p.gap),
                };
                if est_down - ideal > p.tolerance {
                    late_dropped += 1; // never push a note back: remove it
                    continue;
                }
                played[id] = true;
                if switch {
                    if let Some(s) = mod_scan(modstate) {
                        g.emit(at, Act::Up(s), None);
                        g.up_t[s as usize] = at;
                    }
                    if let Some(s) = mod_scan(k.modifier) {
                        g.emit(at, Act::Down(s), None);
                    }
                    modstate = k.modifier;
                    mod_since = at;
                }
                // key still down (repeat, or t/T sharing one key): re-press it
                if let Some(owner) = g.owner[sc] {
                    g.release(sc, at - p.gap);
                    if owner != pitch {
                        g.refs[owner as usize] = 0;
                        g.s.cut_by_clash += 1;
                    }
                }
                let down = at.max(g.up_t[sc] + p.gap);
                g.emit(down, Act::Down(k.scan), Some((pitch, k.modifier)));
                g.owner[sc] = Some(pitch);
                g.down_t[sc] = down;
                g.vis_of[sc] = g.vis.len();
                g.vis.push(VisNote { pitch, start: down, end: down, kept: true });
                note_played[path[id].0] = true;
                g.refs[pitch as usize] += 1;
                last_down = last_down.max(down);
                let late = down - ideal;
                g.s.strikes += 1;
                g.s.sum_late += late;
                g.s.max_late = g.s.max_late.max(late);
                if late > 1000 {
                    g.s.delayed += 1;
                }
            }
        }
    }

    // song end: release everything still down
    let mut fin = g.out.iter().map(|x| x.0).max().unwrap_or(0);
    for sc in 0..128 {
        if g.owner[sc].is_some() {
            fin = fin.max(g.down_t[sc] + p.hold);
            g.emit(fin, Act::Up(sc as u16), None);
            if let Some(v) = g.vis.get_mut(g.vis_of[sc]) {
                v.end = fin;
            }
        }
    }
    if let Some(s) = mod_scan(modstate) {
        g.emit(fin.max(last_down + p.dwell), Act::Up(s), None);
    }
    if pedal {
        g.emit(fin.max(pedal_t + p.gap), Act::Up(SC_SPACE), None);
    }

    let mut vis = std::mem::take(&mut g.vis);
    for (i, n) in notes.iter().enumerate() {
        if !note_played[i] {
            vis.push(VisNote { pitch: n.pitch, start: n.t, end: n.off.max(n.t + 30_000), kept: false });
        }
    }
    vis.sort_by_key(|v| v.start);

    let mut out = std::mem::take(&mut g.out);
    out.sort_by_key(|x| (x.0, x.1));
    let mut s = g.s;
    s.actions = out.into_iter().map(|x| x.2).collect();
    s.vis = vis;
    s.notes = n_on;
    s.merged = merged;
    s.dropped = notes.len() - path.len() + late_dropped;
    s
}

/// Independent checker: replays the timeline and verifies every rule.
pub fn verify(actions: &[Action], p: Params) -> Result<(), String> {
    let mut down = [false; 256];
    let mut last_down = [NEG; 256];
    let mut last_up = [NEG; 256];
    let mut prev_t = NEG;
    let mut last_note_down = NEG; // last note key-down under the current modifier state
    for (idx, a) in actions.iter().enumerate() {
        if a.t < prev_t {
            return Err(format!("#{idx}: 시간 역순"));
        }
        prev_t = a.t;
        match a.act {
            Act::Down(s) => {
                let s = s as usize;
                if down[s] {
                    return Err(format!("#{idx}: 이미 눌린 키를 다시 누름 (scan {s:#x})"));
                }
                if a.t - last_up[s] < p.gap {
                    return Err(format!("#{idx}: 재타건 간격 부족 (scan {s:#x})"));
                }
                if s as u16 == SC_LSHIFT || s as u16 == SC_LCTRL {
                    if a.t - last_note_down < p.dwell {
                        return Err(format!("#{idx}: 수식키 전환이 너무 빠름"));
                    }
                }
                down[s] = true;
                last_down[s] = a.t;
                if let Some((pitch, m)) = a.note {
                    let shift = down[SC_LSHIFT as usize];
                    let ctrl = down[SC_LCTRL as usize];
                    let state = match (shift, ctrl) {
                        (false, false) => Mod::None,
                        (true, false) => Mod::Shift,
                        (false, true) => Mod::Ctrl,
                        _ => return Err(format!("#{idx}: Shift와 Ctrl 동시 눌림")),
                    };
                    if state != m {
                        return Err(format!("#{idx}: 음 {pitch} 의 수식키 상태가 틀림"));
                    }
                    last_note_down = a.t;
                }
            }
            Act::Up(s) => {
                let s = s as usize;
                if !down[s] {
                    return Err(format!("#{idx}: 눌리지 않은 키를 뗌 (scan {s:#x})"));
                }
                if s as u16 == SC_LSHIFT || s as u16 == SC_LCTRL {
                    if a.t - last_note_down < p.dwell {
                        return Err(format!("#{idx}: 수식키를 너무 일찍 뗌"));
                    }
                } else if a.t - last_down[s] < p.hold {
                    return Err(format!("#{idx}: 누름 시간 부족 (scan {s:#x})"));
                }
                down[s] = false;
                last_up[s] = a.t;
            }
        }
    }
    if let Some(s) = down.iter().position(|&d| d) {
        return Err(format!("끝났는데 눌린 키가 남음 (scan {s:#x})"));
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    const P: Params = Params { dwell: 20_000, hold: 0, gap: 0, tolerance: 20_000, rate_count: 0, rate_window: 100_000 };

    fn on(t: i64, n: u8) -> Ev { Ev { t, kind: Kind::NoteOn(n) } }
    fn off(t: i64, n: u8) -> Ev { Ev { t, kind: Kind::NoteOff(n) } }

    fn sorted(mut v: Vec<Ev>) -> Vec<Ev> {
        v.sort_by_key(|e| (e.t, match e.kind { Kind::NoteOff(_) => 0, Kind::Pedal(_) => 1, Kind::NoteOn(_) => 2 }));
        v
    }
    fn song(step_us: i64, n: usize, pitches: &[u8]) -> Vec<Ev> {
        let mut v = vec![];
        for i in 0..n {
            let t = i as i64 * step_us;
            let p = pitches[i % pitches.len()];
            v.push(on(t, p));
            v.push(off(t + step_us - 500, p));
        }
        sorted(v)
    }
    fn check(ev: &[Ev], p: Params) -> Schedule {
        let s = build(ev, p);
        verify(&s.actions, p).unwrap();
        assert_eq!(s.strikes + s.merged + s.dropped, s.notes, "every note accounted for");
        assert!(s.max_late <= p.tolerance, "late {}", s.max_late);
        assert_eq!(s.vis.iter().filter(|v| v.kept).count(), s.strikes);
        assert_eq!(s.vis.len(), s.strikes + s.dropped);
        assert!(s.vis.iter().all(|v| v.end >= v.start));
        s
    }

    #[test]
    fn plain_melody_is_exact() {
        let s = check(&song(100_000, 50, &[60, 62, 64, 65, 67]), P);
        assert_eq!((s.max_late, s.dropped), (0, 0));
    }

    #[test]
    fn mixed_chord_is_rolled_not_dropped() {
        // C4 + C#4 + E4 at once (C4 `t` and C#4 `T` even share a key)
        let ev = sorted(vec![on(0, 60), on(0, 61), on(0, 64), off(500_000, 60), off(500_000, 61), off(500_000, 64)]);
        let s = check(&ev, P);
        assert_eq!((s.strikes, s.dropped), (3, 0));
    }

    #[test]
    fn black_key_run_keeps_shift_down() {
        let s = check(&song(10_000, 5, &[61, 63, 66, 68, 70]), P);
        let shifts = s.actions.iter().filter(|a| a.act == Act::Down(SC_LSHIFT)).count();
        assert_eq!((shifts, s.max_late, s.dropped), (1, 0, 0));
    }

    #[test]
    fn very_fast_repeats_are_not_throttled() {
        // same key 200 times at 4 ms intervals: every one is pressed, on time
        let s = check(&song(4_000, 200, &[60]), P);
        assert_eq!((s.strikes, s.max_late), (200, 0));
    }

    #[test]
    fn normal_fast_piece_loses_nothing() {
        // 16ths at 160 BPM mixing white and black keys
        let s = check(&song(93_750, 2_000, &[60, 62, 63, 65, 67, 68, 70, 72, 74, 75]), P);
        assert_eq!(s.dropped, 0);
    }

    /// Black-MIDI-like: white/black/ctrl notes every 2 ms for 60 s. Nothing may
    /// drift: the kept notes stay in place, the rest are removed.
    #[test]
    fn dense_stream_keeps_notes_in_place() {
        let ev = song(2_000, 30_000, &[60, 61, 22, 64, 66, 100, 67, 70]);
        let s = check(&ev, P);
        assert!(s.dropped > 0 && s.strikes > 1000);
        let last = s.actions.iter().filter(|a| a.note.is_some()).map(|a| a.t).max().unwrap();
        assert!(last >= 59_900_000 && last <= 60_000_000 + P.tolerance, "{last}");
    }

    /// The chooser must beat a naive "always serve current state" approach:
    /// a white melody plus a dense black cluster -> it switches for the cluster.
    #[test]
    fn chooser_prefers_the_larger_group() {
        let mut v = vec![];
        // white notes every 15 ms from 0..300 ms; 10 black notes at 150 ms (a cluster)
        for i in 0..20 { v.push(on(i * 15_000, 60 + (i % 2) as u8 * 2)); v.push(off(i * 15_000 + 10_000, 60 + (i % 2) as u8 * 2)); }
        for n in [61u8, 63, 66, 68, 70, 73, 75, 78, 80, 82] { v.push(on(150_000, n)); v.push(off(200_000, n)); }
        let s = check(&sorted(v), P);
        // the cluster (10 notes) is worth losing ~3 white notes for
        let blacks = s.actions.iter().filter(|a| matches!(a.note, Some((_, Mod::Shift)))).count();
        assert_eq!(blacks, 10);
        assert!(s.dropped <= 4, "dropped {}", s.dropped);
    }

    #[test]
    fn rate_limit_keeps_melody_and_bass() {
        // 10-note chords every 10 ms = 1000 notes/s, limited to 15 per 100 ms
        let mut v = vec![];
        for i in 0..300i64 {
            for n in [36u8, 40, 43, 48, 52, 55, 60, 64, 67, 72] { v.push(on(i * 10_000, n)); v.push(off(i * 10_000 + 9_000, n)); }
        }
        let p = Params { rate_count: 15, rate_window: 100_000, ..P };
        let s = check(&sorted(v), p);
        // no 100 ms window ever holds more than 15 strikes
        let downs: Vec<i64> = s.actions.iter().filter(|a| a.note.is_some()).map(|a| a.t).collect();
        for (k, &t) in downs.iter().enumerate() {
            assert!(downs[k..].iter().take_while(|&&x| x < t + 100_000).count() <= 15);
        }
        // the limit is used, not over-thinned
        assert!(downs.len() >= 15 * 28, "kept {}", downs.len());
        // the top and bottom voices survive the most
        let count = |pt: u8| s.actions.iter().filter(|a| matches!(a.note, Some((q, _)) if q == pt)).count();
        assert!(count(72) >= count(55) && count(36) >= count(55));
        assert!(count(72) > 250, "melody kept nearly every time: {}", count(72));
    }

    #[test]
    fn rate_limit_on_empty_song() {
        let s = check(&[], Params { rate_count: 20, rate_window: 100_000, ..P });
        assert_eq!(s.strikes, 0);
    }

    #[test]
    fn rate_limit_window_is_user_set() {
        // 5-note chord every 50 ms: 2 chords per 100 ms window
        let mut v = vec![];
        for i in 0..40i64 {
            for n in [48u8, 52, 55, 60, 64] { v.push(on(i * 50_000, n)); v.push(off(i * 50_000 + 40_000, n)); }
        }
        let ev = sorted(v);
        // 10 per 100 ms: everything fits
        assert_eq!(check(&ev, Params { rate_count: 10, rate_window: 100_000, ..P }).dropped, 0);
        // same count over a 200 ms window: about half must go
        let s = check(&ev, Params { rate_count: 10, rate_window: 200_000, ..P });
        let downs: Vec<i64> = s.actions.iter().filter(|a| a.note.is_some()).map(|a| a.t).collect();
        for (k, &t) in downs.iter().enumerate() {
            assert!(downs[k..].iter().take_while(|&&x| x < t + 200_000).count() <= 10);
        }
        assert!((90..=100).contains(&s.strikes), "kept {}", s.strikes);
    }

    #[test]
    fn pedal_and_88_range() {
        let ev = sorted(vec![
            Ev { t: 0, kind: Kind::Pedal(true) }, on(0, 21), on(0, 108), on(0, 60),
            Ev { t: 300_000, kind: Kind::Pedal(false) }, Ev { t: 300_500, kind: Kind::Pedal(true) },
            off(400_000, 21), off(400_000, 108), off(400_000, 60),
            Ev { t: 500_000, kind: Kind::Pedal(false) },
        ]);
        let s = check(&ev, P);
        assert_eq!(s.dropped, 0); // ctrl+none chord: rolled by one switch, within tolerance
    }

    #[test]
    fn random_stress() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut rnd = |m: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % m
        };
        for round in 0..400 {
            let mut v = vec![];
            let mut t = 0i64;
            let dense = round % 2 == 0;
            for _ in 0..400 {
                t += if dense { rnd(3) as i64 * rnd(4_000) as i64 } else { rnd(4) as i64 * rnd(15_000) as i64 };
                let n = 21 + rnd(88) as u8;
                let d = 1 + rnd(200_000) as i64;
                v.push(on(t, n));
                v.push(off(t + d, n));
                if rnd(10) == 0 {
                    v.push(Ev { t, kind: Kind::Pedal(rnd(2) == 0) });
                }
            }
            let tol = [0, 10_000, 20_000, 45_000][round % 4];
            let hg = [0, 0, 1_000, 5_000][(round / 4) % 4];
            check(&sorted(v), Params { tolerance: tol, hold: hg, gap: hg, ..P });
        }
    }

    #[test]
    fn checker_rejects_unsafe_timeline() {
        let ev = sorted(vec![on(0, 60), on(0, 61), on(5_000, 62), off(100_000, 60), off(100_000, 61), off(100_000, 62)]);
        let bad = build(&ev, Params { dwell: 0, ..P });
        assert!(verify(&bad.actions, P).is_err());
        verify(&build(&ev, P).actions, P).unwrap();
    }
}
