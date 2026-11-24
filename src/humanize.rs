//! Optional "human" feel: small deliberate timing errors and slightly staggered
//! chords, applied to the MIDI events *before* scheduling. The scheduler then
//! treats the humanized times as the ideal ones, so every Shift/Ctrl rule and
//! the "never push a note back" rule still hold.
//!
//! - Each musical moment (notes within CHORD_US of each other) is shifted by one
//!   random offset (normal, sd `jitter`), capped so neighbouring moments never
//!   swap order: a fast run stays a run, just less mechanical.
//! - In a chord the highest note (melody) goes first and the rest follow within
//!   `lead/2..lead` (pianists' melody lead is about 20-30 ms; Goebl 2001, JASA).
//!   The followers are grouped by Shift/Ctrl state, so a chord never needs more
//!   modifier switches than it would unhumanized.
//! - Note lengths are kept, except that a note which ended by the next moment
//!   still ends by that moment's new start (released before its Shift/Ctrl
//!   switch); pedal events are not moved.
//! - Rubato: the song is cut into 3-8 s stretches; in each the tempo drifts
//!   gradually up or down to `rubato` (fraction) and then snaps back over the
//!   last fifth. Expressive piano timing varies by about 10% (eighth-note IOI
//!   sd 9.5% in Timmers et al. 2003); "strong" uses that, lower levels less.
//! - Slips: notes in hard spots (fast notes in one hand, big leaps, inner chord
//!   voices) sometimes go wrong: left out, a neighbouring key grazed as well, or
//!   the neighbouring key instead (split about 43/39/18%, as in Flossmann &
//!   Widmer 2011). The melody (top note) slips least (Palmer & van de Sande 1993).
//!   How often is our choice; no study gives a per-note rate for this.
//! - The random sequence is fixed, so the same file and settings always give the
//!   same performance (and the visualizer shows exactly what is played).

use crate::layout::key_for;
use crate::midi::{class, Ev, Kind};

/// notes this close together count as one chord
pub const CHORD_US: i64 = 3_000;
/// labels for the GUI / report, index = level
pub const LEVEL_NAMES: [&str; 4] = ["끔", "약", "중", "강"];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Human {
    /// standard deviation of the per-moment timing error (us)
    pub jitter: i64,
    /// how far behind the melody note the rest of a chord may come (us)
    pub lead: i64,
    /// largest tempo drift of a rubato stretch (0.06 = 6%)
    pub rubato: f64,
    /// slip probability per point of difficulty (a note scores 0-3)
    pub slip: f64,
}

/// What the slips pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Slips {
    pub missed: usize,
    pub grazed: usize,
    pub wrong: usize,
}

/// 0 = off, 1..=3 = light / medium / strong
pub fn level(n: u8) -> Option<Human> {
    match n {
        1 => Some(Human { jitter: 3_000, lead: 10_000, rubato: 0.03, slip: 0.002 }),
        2 => Some(Human { jitter: 6_000, lead: 20_000, rubato: 0.06, slip: 0.006 }),
        3 => Some(Human { jitter: 10_000, lead: 30_000, rubato: 0.10, slip: 0.015 }),
        _ => None,
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// uniform in [0, 1)
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn normal(&mut self) -> f64 {
        let u = 1.0 - self.unit(); // (0, 1]
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * self.unit()).cos()
    }
}

const SEED: u64 = 0x6B65_7962_6C6F_7821;

/// Rubato, then slips, then per-moment timing (see the module comment).
pub fn apply(events: &[Ev], h: Human) -> (Vec<Ev>, Slips) {
    let ev = rubato(events, h.rubato);
    let (ev, slips) = slip(&ev, h.slip);
    (timing(&ev, h), slips)
}

fn sorted(mut v: Vec<Ev>) -> Vec<Ev> {
    // same order as midi::parse: time, then offs before pedal before ons (stable)
    v.sort_by_key(|e| (e.t, class(e.kind)));
    v
}

/// Time warp: in each stretch the rate of time drifts linearly from 1 to 1+e
/// (e = ±amount) over the first 4/5, then back to 1 over the last 1/5.
/// Always increasing, so event order is kept.
fn rubato(events: &[Ev], amount: f64) -> Vec<Ev> {
    let end = events.last().map(|e| e.t).unwrap_or(0);
    if amount <= 0.0 || end <= 0 {
        return events.to_vec();
    }
    let mut rng = Rng(SEED ^ 0x7275_6261_746F);
    // (start, length, e) per stretch, and the warped time at each start
    let mut segs: Vec<(f64, f64, f64, f64)> = vec![];
    let (mut t, mut w) = (0.0f64, 0.0f64);
    while t <= end as f64 {
        let len = 3e6 + 5e6 * rng.unit();
        let e = amount * (0.5 + 0.5 * rng.unit()) * if rng.next() & 1 == 0 { 1.0 } else { -1.0 };
        segs.push((t, len, e, w));
        w += len + e * len / 2.0; // area of the triangle-shaped drift
        t += len;
    }
    const RISE: f64 = 0.8;
    let warp = |x: i64| -> i64 {
        let x = x as f64;
        let k = segs.partition_point(|s| s.0 <= x).saturating_sub(1);
        let (s0, len, e, w0) = segs[k];
        let u = x - s0;
        let up = RISE * len;
        let extra = if u <= up {
            e * u * u / (2.0 * up)
        } else {
            let v = u - up;
            e * up / 2.0 + e * (v - v * v / (2.0 * (len - up)))
        };
        (w0 + u + extra).round() as i64
    };
    sorted(events.iter().map(|e| Ev { t: warp(e.t), kind: e.kind }).collect())
}

/// One note: start, end, pitch.
#[derive(Clone, Copy)]
struct Note {
    on: i64,
    off: i64,
    pitch: u8,
}

fn pair(events: &[Ev]) -> (Vec<Note>, Vec<Ev>) {
    let mut notes: Vec<Note> = vec![];
    let mut open: Vec<std::collections::VecDeque<usize>> = vec![Default::default(); 128];
    let mut pedal = vec![];
    let end = events.last().map(|e| e.t).unwrap_or(0);
    for e in events {
        match e.kind {
            Kind::NoteOn(p) => {
                notes.push(Note { on: e.t, off: end, pitch: p });
                open[p as usize].push_back(notes.len() - 1);
            }
            Kind::NoteOff(p) => {
                if let Some(i) = open[p as usize].pop_front() {
                    notes[i].off = e.t;
                }
            }
            Kind::Pedal(_) => pedal.push(*e),
        }
    }
    (notes, pedal)
}

fn unpair(notes: &[Note], pedal: Vec<Ev>) -> Vec<Ev> {
    let mut v = pedal;
    for n in notes {
        v.push(Ev { t: n.on, kind: Kind::NoteOn(n.pitch) });
        v.push(Ev { t: n.off, kind: Kind::NoteOff(n.pitch) });
    }
    sorted(v)
}

/// Notes starting within CHORD_US of `t` (notes are sorted by start).
fn same_moment(notes: &[Note], t: i64) -> &[Note] {
    let a = notes.partition_point(|m| m.on < t - CHORD_US);
    let b = notes.partition_point(|m| m.on <= t + CHORD_US);
    &notes[a..b]
}

/// How hard a note is to hit cleanly, 0-3, and whether it is the melody (top
/// note of its moment). Hands are approximated as below / from middle C.
fn difficulty(notes: &[Note], i: usize) -> (u32, bool) {
    let n = notes[i];
    let right = n.pitch >= 60;
    let moment = same_moment(notes, n.on);
    let hi = moment.iter().map(|m| m.pitch).max().unwrap();
    let lo = moment.iter().map(|m| m.pitch).min().unwrap();
    let mut d = 0;
    if moment.len() >= 3 && n.pitch != hi && n.pitch != lo {
        d += 1; // inner voice
    }
    // the previous note in the same hand, from an earlier moment
    let prev = notes[..i].iter().rev().take(64).find(|m| (m.pitch >= 60) == right && n.on - m.on > CHORD_US);
    if let Some(m) = prev {
        let gap = n.on - m.on;
        if gap < 90_000 {
            d += 1; // fast notes
        }
        if gap < 200_000 && (n.pitch as i32 - m.pitch as i32).abs() > 12 {
            d += 1; // leap
        }
    }
    (d, n.pitch == hi && moment.len() >= 2 || moment.len() == 1 && right)
}

fn slip(events: &[Ev], rate: f64) -> (Vec<Ev>, Slips) {
    let mut st = Slips::default();
    if rate <= 0.0 {
        return (events.to_vec(), st);
    }
    let (notes, pedal) = pair(events);
    let mut rng = Rng(SEED ^ 0x736C_6970);
    let mut out: Vec<Note> = Vec::with_capacity(notes.len());
    for i in 0..notes.len() {
        let n = notes[i];
        let (d, melody) = difficulty(&notes, i);
        let p = rate * d as f64 * if melody { 0.3 } else { 1.0 };
        if d == 0 || rng.unit() >= p {
            out.push(n);
            continue;
        }
        // a neighbouring key (a semitone off) that is not already in this moment
        let up = rng.next() & 1 == 0;
        let near = [if up { 1 } else { -1 }, if up { -1 } else { 1 }]
            .into_iter()
            .map(|s| n.pitch as i32 + s)
            .find(|&q| (21..=108).contains(&q) && !same_moment(&notes, n.on).iter().any(|m| m.pitch as i32 == q))
            .map(|q| q as u8);
        let r = rng.unit();
        match near {
            _ if r < 0.43 => st.missed += 1, // left out
            Some(q) if r < 0.82 => {
                out.push(n);
                out.push(Note { on: n.on, off: n.on + (n.off - n.on).min(60_000), pitch: q });
                st.grazed += 1;
            }
            Some(q) => {
                out.push(Note { pitch: q, ..n });
                st.wrong += 1;
            }
            None => out.push(n),
        }
    }
    (unpair(&out, pedal), st)
}

fn timing(events: &[Ev], h: Human) -> Vec<Ev> {
    let mut rng = Rng(SEED);
    // note-on indices in time order, and each one's shift
    let ons: Vec<usize> = (0..events.len()).filter(|&i| matches!(events[i].kind, Kind::NoteOn(_))).collect();
    let mut shift = vec![0i64; events.len()];

    // chords: runs of note-ons within CHORD_US of the run's first note
    let mut chords: Vec<(usize, usize)> = vec![]; // [a, b) into `ons`
    let mut a = 0;
    while a < ons.len() {
        let t0 = events[ons[a]].t;
        let mut b = a;
        while b < ons.len() && events[ons[b]].t - t0 <= CHORD_US {
            b += 1;
        }
        chords.push((a, b));
        a = b;
    }

    let pitch = |i: usize| match events[i].kind {
        Kind::NoteOn(p) => p,
        _ => unreachable!(),
    };
    // each moment's original time and new start (its first, melody note)
    let mut starts: Vec<(i64, i64)> = Vec::with_capacity(chords.len());
    for (c, &(a, b)) in chords.iter().enumerate() {
        let t = events[ons[a]].t;
        let prev = if c > 0 { t - events[ons[chords[c - 1].0]].t } else { i64::MAX };
        let next = chords.get(c + 1).map(|&(n, _)| events[ons[n]].t - events[ons[b - 1]].t).unwrap_or(i64::MAX);
        // keep moments in order: jitter <= 1/4 of the gap on either side,
        // chord spread <= 2/5 of the gap after it (1/4 + 2/5 + 1/4 < 1)
        let cap = ((h.jitter * 5 / 2) as f64).min(prev.min(next) as f64 / 4.0);
        let j = (rng.normal() * h.jitter as f64).clamp(-cap, cap).round() as i64;
        starts.push((t, (t + j).max(0)));

        let mut idx: Vec<usize> = ons[a..b].to_vec();
        let mut offs = vec![0i64; idx.len()];
        if idx.len() > 1 {
            let lead = (h.lead as f64).min(next as f64 * 0.4);
            // melody (highest) first, then its own modifier group, then the others;
            // random order inside each group
            idx.sort_by_key(|&i| std::cmp::Reverse(pitch(i)));
            let melody_mod = key_for(pitch(idx[0])).unwrap().modifier;
            let mut rest: Vec<(u8, u64, usize)> = idx[1..]
                .iter()
                .map(|&i| {
                    let m = key_for(pitch(i)).unwrap().modifier;
                    (if m == melody_mod { 0 } else { 1 + m as u8 }, rng.next(), i)
                })
                .collect();
            rest.sort();
            for (k, r) in rest.iter().enumerate() {
                idx[k + 1] = r.2;
            }
            let mut f: Vec<i64> = (1..idx.len()).map(|_| (lead * (0.5 + 0.5 * rng.unit())).round() as i64).collect();
            f.sort();
            offs[1..].copy_from_slice(&f);
        }
        for (k, &i) in idx.iter().enumerate() {
            // chord notes start together from the chord's time, then spread;
            // a pitch doubled by layered tracks stays one note (same shift)
            let twin = idx[..k].iter().find(|&&o| pitch(o) == pitch(i)).map(|&o| shift[o]);
            shift[i] = twin.unwrap_or(t - events[i].t + j + offs[k]);
        }
    }

    // move each note-off with its note-on (FIFO per pitch, like the scheduler),
    // but never past the new start of the moment it originally ended before or
    // at: a legato note must still be released before the next moment's
    // Shift/Ctrl switch, or the key comes up under the other modifier state
    let mut open: Vec<std::collections::VecDeque<(i64, i64)>> = vec![Default::default(); 128];
    let mut out: Vec<Ev> = Vec::with_capacity(events.len());
    for (i, e) in events.iter().enumerate() {
        match e.kind {
            Kind::NoteOn(p) => {
                let t = (e.t + shift[i]).max(0);
                open[p as usize].push_back((t, t - e.t));
                out.push(Ev { t, kind: e.kind });
            }
            Kind::NoteOff(p) => {
                let (on_t, d) = open[p as usize].pop_front().unwrap_or((0, 0));
                let mut t = (e.t + d).max(0);
                let k = starts.partition_point(|s| s.0 < e.t);
                if let Some(&(_, next_start)) = starts.get(k) {
                    t = t.min(next_start).max(on_t);
                }
                out.push(Ev { t, kind: e.kind });
            }
            Kind::Pedal(_) => out.push(*e),
        }
    }
    sorted(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Mod;
    use crate::schedule;

    /// timing only, so the timing tests see exact positions
    const H: Human = Human { jitter: 6_000, lead: 20_000, rubato: 0.0, slip: 0.0 };

    /// largest shift `apply` can give any note
    fn max_shift(h: Human) -> i64 { h.jitter * 5 / 2 + h.lead }

    fn on(t: i64, n: u8) -> Ev { Ev { t, kind: Kind::NoteOn(n) } }
    fn off(t: i64, n: u8) -> Ev { Ev { t, kind: Kind::NoteOff(n) } }
    fn sorted(mut v: Vec<Ev>) -> Vec<Ev> {
        v.sort_by_key(|e| (e.t, class(e.kind)));
        v
    }
    fn ons(ev: &[Ev]) -> Vec<(i64, u8)> {
        ev.iter().filter_map(|e| if let Kind::NoteOn(p) = e.kind { Some((e.t, p)) } else { None }).collect()
    }
    /// (on, off) per note, matched FIFO per pitch, in note-on order
    fn notes(ev: &[Ev]) -> Vec<(u8, i64, i64)> {
        let mut open: Vec<std::collections::VecDeque<usize>> = vec![Default::default(); 128];
        let mut v: Vec<(u8, i64, i64)> = vec![];
        for e in ev {
            match e.kind {
                Kind::NoteOn(p) => { v.push((p, e.t, -1)); open[p as usize].push_back(v.len() - 1); }
                Kind::NoteOff(p) => { if let Some(i) = open[p as usize].pop_front() { v[i].2 = e.t; } }
                _ => {}
            }
        }
        v
    }

    #[test]
    fn levels() {
        assert_eq!(level(0), None);
        assert!(level(1).unwrap().jitter < level(3).unwrap().jitter);
        assert_eq!(LEVEL_NAMES.len(), 4);
    }

    #[test]
    fn runs_stay_in_order_and_lengths_are_kept() {
        // 16ths at 150 ms, then a fast 20 ms run: notes move, but never swap order
        let mut v = vec![];
        for i in 0..200i64 {
            let t = if i < 100 { i * 150_000 } else { 15_000_000 + (i - 100) * 20_000 };
            let p = 60 + (i % 12) as u8;
            v.push(on(t, p));
            v.push(off(t + 15_000, p));
        }
        let ev = sorted(v);
        let hv = apply(&ev, H).0;
        assert_eq!(hv.len(), ev.len());
        let (a, b) = (notes(&ev), notes(&hv));
        let mut moved = 0;
        for (k, (x, y)) in a.iter().zip(&b).enumerate() {
            assert_eq!(x.0, y.0, "order of notes changed");
            // same length, unless that would run past the next note's new start
            let next = b.get(k + 1).map(|n| n.1).unwrap_or(i64::MAX);
            assert!(y.2 - y.1 == x.2 - x.1 || (y.2 - y.1 < x.2 - x.1 && y.2 == next), "length changed");
            assert!((y.1 - x.1).abs() <= max_shift(H));
            moved += (y.1 != x.1) as usize;
        }
        assert!(moved > 150, "most notes get a small error: {moved}");
        // in the 20 ms run the error is capped at 1/4 of the gap
        for (x, y) in a[100..].iter().zip(&b[100..]) {
            assert!((y.1 - x.1).abs() <= 5_000);
        }
    }

    #[test]
    fn chord_melody_leads_and_is_staggered() {
        // C major + melody E5, and a mixed chord C4 C#4 E4 G4 (C4 `t`, C#4 `T`)
        let ev = sorted(vec![
            on(1_000_000, 48), on(1_000_000, 55), on(1_000_000, 60), on(1_000_000, 76),
            off(1_500_000, 48), off(1_500_000, 55), off(1_500_000, 60), off(1_500_000, 76),
            on(2_000_000, 60), on(2_000_000, 61), on(2_000_000, 64), on(2_000_000, 67),
            off(2_500_000, 60), off(2_500_000, 61), off(2_500_000, 64), off(2_500_000, 67),
        ]);
        let hv = apply(&ev, H).0;
        let o = ons(&hv);
        let first = &o[..4];
        assert_eq!(first[0].1, 76, "melody first");
        assert!(first[1].0 - first[0].0 >= H.lead / 2 && first[3].0 - first[0].0 <= H.lead);
        // second chord: melody G4 first; the plain (white) notes stay together, then Shift
        let second: Vec<Mod> = o[4..].iter().map(|x| key_for(x.1).unwrap().modifier).collect();
        assert_eq!(o[4].1, 67);
        assert_eq!(second, vec![Mod::None, Mod::None, Mod::None, Mod::Shift]);
    }

    #[test]
    fn doubled_notes_stay_merged() {
        // the same pitch from two tracks at one instant is struck once, as without humanize
        let ev = sorted(vec![on(0, 60), on(0, 64), on(0, 67), on(0, 67), off(400_000, 60), off(400_000, 64), off(400_000, 67), off(400_000, 67)]);
        let p = schedule::Params::default();
        let s = schedule::build(&apply(&ev, H).0, p);
        assert_eq!((s.strikes, s.merged), (3, 1));
    }

    /// Share of note keys released under a different Shift/Ctrl state than they
    /// were pressed under (a Roblox piano may read the state again on release).
    fn release_mismatch(ev: &[Ev]) -> usize {
        use crate::layout::{SC_LCTRL, SC_LSHIFT};
        let s = schedule::build(ev, schedule::Params::default());
        let (mut shift, mut ctrl) = (false, false);
        let mut at_down = [(false, false); 256];
        let mut bad = 0;
        for a in &s.actions {
            match a.act {
                schedule::Act::Down(sc) if sc == SC_LSHIFT => shift = true,
                schedule::Act::Up(sc) if sc == SC_LSHIFT => shift = false,
                schedule::Act::Down(sc) if sc == SC_LCTRL => ctrl = true,
                schedule::Act::Up(sc) if sc == SC_LCTRL => ctrl = false,
                schedule::Act::Down(sc) => at_down[sc as usize] = (shift, ctrl),
                schedule::Act::Up(sc) => bad += (at_down[sc as usize] != (shift, ctrl)) as usize,
            }
        }
        bad
    }

    /// In a quantized legato line each note ends exactly where the next begins, so
    /// unhumanized it is released before the next note's Shift/Ctrl switch. Humanize
    /// must keep that: the old note may not end after the next moment has started.
    #[test]
    fn legato_releases_before_the_next_switch() {
        let mut v = vec![];
        let line = [60u8, 61, 62, 63, 64, 66, 67, 68, 69, 70, 71, 72];
        for i in 0..400i64 {
            let (t, p) = (i * 125_000, line[(i % 12) as usize]);
            v.push(on(t, p));
            v.push(off(t + 125_000, p));
            if i % 2 == 0 { v.push(on(t, 48 + (i % 3) as u8)); v.push(off(t + 250_000, 48 + (i % 3) as u8)); }
        }
        let ev = sorted(v);
        let base = release_mismatch(&ev);
        for lv in 1..=3 {
            let hv = release_mismatch(&apply(&ev, level(lv).unwrap()).0);
            assert!(hv <= base, "level {lv}: {hv} releases under another modifier (unhumanized {base})");
        }
    }

    /// Rubato: the tempo drifts both ways but never more than `rubato`, and it
    /// comes back to the written tempo between stretches.
    #[test]
    fn rubato_drifts_within_bounds_and_returns() {
        let ev = sorted((0..1200i64).flat_map(|i| [on(i * 50_000, 60 + (i % 7) as u8), off(i * 50_000 + 40_000, 60 + (i % 7) as u8)]).collect());
        let a = 0.10;
        let w = ons(&rubato(&ev, a));
        let ratio: Vec<f64> = w.windows(2).map(|p| (p[1].0 - p[0].0) as f64 / 50_000.0).collect();
        assert!(ratio.iter().all(|&r| r > 1.0 - a - 0.001 && r < 1.0 + a + 0.001));
        assert!(ratio.iter().any(|&r| r > 1.0 + a * 0.4) && ratio.iter().any(|&r| r < 1.0 - a * 0.4), "drifts both ways");
        let back = ratio.iter().filter(|&&r| (r - 1.0).abs() < 0.005).count();
        assert!(back >= 5, "returns to the written tempo: {back}");
        // the length changes by at most half the drift
        let len = w.last().unwrap().0 as f64 / 59_950_000.0;
        assert!((len - 1.0).abs() <= a / 2.0, "{len}");
    }

    /// Slips land in hard spots only: an easy slow melody never slips; a fast
    /// leaping passage with inner voices does, and inner voices more than the top.
    #[test]
    fn slips_only_where_it_is_hard() {
        let easy = sorted((0..400i64).flat_map(|i| [on(i * 400_000, 60 + (i % 5) as u8), off(i * 400_000 + 380_000, 60 + (i % 5) as u8)]).collect());
        let (_, s) = slip(&easy, 0.05);
        assert_eq!(s, Slips::default());

        // 3-note chords every 70 ms, jumping 2 octaves back and forth
        let mut v = vec![];
        for i in 0..3000i64 {
            let b = if i % 2 == 0 { 48u8 } else { 74 };
            for p in [b, b + 4, b + 7] { v.push(on(i * 70_000, p)); v.push(off(i * 70_000 + 60_000, p)); }
        }
        let hard = sorted(v);
        let (out, s) = slip(&hard, 0.02);
        assert!(s.missed > 0 && s.grazed > 0 && s.wrong > 0, "{s:?}");
        assert_eq!(ons(&out).len(), ons(&hard).len() - s.missed + s.grazed);
        // inner voice (b+4) slips more than the top (b+7)
        let kept = |q: u8| ons(&out).iter().filter(|x| x.1 == q || x.1 == q + 26).count();
        assert!(kept(52) < kept(55), "inner {} top {}", kept(52), kept(55));
    }

    #[test]
    fn same_result_every_time() {
        let ev = sorted((0..50).flat_map(|i| [on(i * 100_000, 60 + (i % 5) as u8), off(i * 100_000 + 90_000, 60 + (i % 5) as u8)]).collect());
        let h = level(3).unwrap();
        assert_eq!(apply(&ev, h), apply(&ev, h));
    }

    #[test]
    fn humanized_songs_still_schedule_safely() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = |m: u64| { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed % m };
        for round in 0..100 {
            let mut v = vec![];
            let mut t = 0i64;
            for _ in 0..300 {
                t += rnd(4) as i64 * rnd(40_000) as i64;
                let n = 21 + rnd(88) as u8;
                v.push(on(t, n));
                v.push(off(t + 1 + rnd(200_000) as i64, n));
                if rnd(10) == 0 { v.push(Ev { t, kind: Kind::Pedal(rnd(2) == 0) }); }
            }
            let h = level(1 + (round % 3) as u8).unwrap();
            let hv = apply(&sorted(v), h).0;
            let p = schedule::Params::default();
            let s = schedule::build(&hv, p);
            schedule::verify(&s.actions, p).unwrap();
            assert!(s.max_late <= p.tolerance);
        }
    }
}
