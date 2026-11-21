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
//! - Note lengths are kept; pedal events are not moved.
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
}

/// 0 = off, 1..=3 = light / medium / strong
pub fn level(n: u8) -> Option<Human> {
    match n {
        1 => Some(Human { jitter: 3_000, lead: 10_000 }),
        2 => Some(Human { jitter: 6_000, lead: 20_000 }),
        3 => Some(Human { jitter: 10_000, lead: 30_000 }),
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

pub fn apply(events: &[Ev], h: Human) -> Vec<Ev> {
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
    for (c, &(a, b)) in chords.iter().enumerate() {
        let t = events[ons[a]].t;
        let prev = if c > 0 { t - events[ons[chords[c - 1].0]].t } else { i64::MAX };
        let next = chords.get(c + 1).map(|&(n, _)| events[ons[n]].t - events[ons[b - 1]].t).unwrap_or(i64::MAX);
        // keep moments in order: jitter <= 1/4 of the gap on either side,
        // chord spread <= 2/5 of the gap after it (1/4 + 2/5 + 1/4 < 1)
        let cap = ((h.jitter * 5 / 2) as f64).min(prev.min(next) as f64 / 4.0);
        let j = (rng.normal() * h.jitter as f64).clamp(-cap, cap).round() as i64;

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

    // move each note-off with its note-on (FIFO per pitch, like the scheduler)
    let mut open: Vec<std::collections::VecDeque<i64>> = vec![Default::default(); 128];
    let mut out: Vec<Ev> = Vec::with_capacity(events.len());
    for (i, e) in events.iter().enumerate() {
        match e.kind {
            Kind::NoteOn(p) => {
                let t = (e.t + shift[i]).max(0);
                open[p as usize].push_back(t - e.t);
                out.push(Ev { t, kind: e.kind });
            }
            Kind::NoteOff(p) => {
                let d = open[p as usize].pop_front().unwrap_or(0);
                out.push(Ev { t: (e.t + d).max(0), kind: e.kind });
            }
            Kind::Pedal(_) => out.push(*e),
        }
    }
    // same order as midi::parse: time, then offs before pedal before ons (stable)
    out.sort_by_key(|e| (e.t, class(e.kind)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Mod;
    use crate::schedule;

    const H: Human = Human { jitter: 6_000, lead: 20_000 };

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
        let hv = apply(&ev, H);
        assert_eq!(hv.len(), ev.len());
        let (a, b) = (notes(&ev), notes(&hv));
        let mut moved = 0;
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.0, y.0, "order of notes changed");
            assert_eq!(x.2 - x.1, y.2 - y.1, "length changed");
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
        let hv = apply(&ev, H);
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
        let p = schedule::Params::from_fps(60.0);
        let s = schedule::build(&apply(&ev, H), p);
        assert_eq!((s.strikes, s.merged), (3, 1));
    }

    #[test]
    fn same_result_every_time() {
        let ev = sorted((0..50).flat_map(|i| [on(i * 100_000, 60 + (i % 5) as u8), off(i * 100_000 + 90_000, 60 + (i % 5) as u8)]).collect());
        assert_eq!(apply(&ev, H), apply(&ev, H));
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
            let hv = apply(&sorted(v), h);
            let p = schedule::Params::from_fps(60.0);
            let s = schedule::build(&hv, p);
            schedule::verify(&s.actions, p).unwrap();
            assert!(s.max_late <= p.tolerance);
        }
    }
}
