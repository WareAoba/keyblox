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
//! - Rubato follows the music: slower where notes are dense, faster where
//!   sparse, and each phrase pushes on in its middle, slows into its end and
//!   starts the next one back in tempo (see `rubato`). Bounded by `rubato`
//!   (fraction); "strong" uses 10%, about the eighth-note IOI sd of 9.5% in
//!   Timmers et al. 2003.
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

/// A gap this long between notes ends a phrase.
fn rest_cap(med_ioi: f64) -> i64 {
    (med_ioi * 4.0).max(600_000.0) as i64
}

/// Time warp driven by the music: a local tempo deviation `e` (positive =
/// slower) is set at each moment and the clock runs at 1+e between moments.
/// - Density: where many more notes come than usual for the song, the player
///   slows down; where few come, they press on (speed/accuracy trade-off;
///   Cancino-Chacon & Grachten 2018 find performers slow down to bring out
///   dense melodic/harmonic events). 4x the song's median density = full `amount`.
/// - Phrase arch: a phrase (notes between rests) starts in tempo, pushes ahead
///   towards just past its middle and slows into its end, then the next phrase
///   is back in tempo at once (Demos et al. 2016: slower at phrase starts and
///   ends, fastest just after halfway).
/// Sizes are bounded by `amount`; the split between the two parts is our choice.
fn rubato(events: &[Ev], amount: f64) -> Vec<Ev> {
    if amount <= 0.0 {
        return events.to_vec();
    }
    // moments: (time, notes)
    let mut mo: Vec<(i64, usize)> = vec![];
    for e in events {
        if let Kind::NoteOn(_) = e.kind {
            match mo.last_mut() {
                Some(m) if e.t - m.0 <= CHORD_US => m.1 += 1,
                _ => mo.push((e.t, 1)),
            }
        }
    }
    if mo.len() < 3 {
        return events.to_vec();
    }
    let n = mo.len();
    let mut iois: Vec<i64> = mo.windows(2).map(|w| w[1].0 - w[0].0).collect();
    iois.sort();
    let med_ioi = iois[iois.len() / 2].max(1_000) as f64;

    // density (notes per second) around each moment, over sounding time only
    const WIN: i64 = 750_000;
    let dens: Vec<f64> = (0..n)
        .map(|k| {
            let a = mo.partition_point(|m| m.0 < mo[k].0 - WIN);
            let b = mo.partition_point(|m| m.0 <= mo[k].0 + WIN);
            let notes: usize = mo[a..b].iter().map(|m| m.1).sum();
            notes as f64 / ((mo[b - 1].0 - mo[a].0) as f64 + med_ioi) * 1e6
        })
        .collect();
    // the song's usual density: median over time (each moment weighted by its step)
    let mut wd: Vec<(f64, i64)> = (0..n).map(|k| (dens[k], mo.get(k + 1).map(|m| m.0 - mo[k].0).unwrap_or(0).min(rest_cap(med_ioi)))).collect();
    wd.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap());
    let half = wd.iter().map(|x| x.1).sum::<i64>() / 2;
    let mut acc = 0;
    let med_d = wd.iter().find(|x| { acc += x.1; acc >= half }).unwrap().0;
    let raw: Vec<f64> = dens.iter().map(|d| (amount * (d / med_d).ln() / 4f64.ln()).clamp(-amount, amount)).collect();
    // gradual: average over +-1 s
    let e_dens: Vec<f64> = (0..n)
        .map(|k| {
            let a = mo.partition_point(|m| m.0 < mo[k].0 - 1_000_000);
            let b = mo.partition_point(|m| m.0 <= mo[k].0 + 1_000_000);
            raw[a..b].iter().sum::<f64>() / (b - a) as f64
        })
        .collect();

    // phrases: split at rests of 4x the usual step (at least 0.6 s), and long
    // stretches without rests at their biggest step every 4-8 s
    let rest = rest_cap(med_ioi);
    let mut ends = vec![false; n];
    let mut s0 = 0;
    for k in 0..n {
        if k + 1 == n || mo[k + 1].0 - mo[k].0 >= rest {
            ends[k] = true;
            s0 = k + 1;
        } else if mo[k + 1].0 - mo[s0].0 > 8_000_000 {
            let a = mo.partition_point(|m| m.0 < mo[s0].0 + 4_000_000).max(s0);
            let cut = (a..=k).max_by_key(|&j| (mo[j + 1].0 - mo[j].0, j)).unwrap();
            ends[cut] = true;
            s0 = cut + 1;
        }
    }
    let mut rng = Rng(SEED ^ 0x7275_6261_746F);
    let mut e = vec![0.0f64; n];
    let mut s0 = 0;
    for k in 0..n {
        if ends[k] {
            let (t0, t1) = (mo[s0].0 as f64, mo[k].0 as f64);
            let len = t1 - t0;
            // short phrases get a smaller arch; a little variety between phrases
            let arch = amount * 0.4 * (len / 3e6).min(1.0) * (0.7 + 0.6 * rng.unit());
            for j in s0..=k {
                let x = if len > 0.0 { (mo[j].0 as f64 - t0) / len } else { 0.0 };
                let push = -(std::f64::consts::PI * x.powf(1.16)).sin(); // fastest at x ~ 0.55
                let ritard = if x > 0.85 { ((x - 0.85) / 0.15).powi(2) * 1.5 } else { 0.0 };
                e[j] = (e_dens[j] + arch * (push + ritard)).clamp(-amount, amount);
            }
            e[s0] = e_dens[s0].clamp(-amount, amount); // back in tempo at once
            s0 = k + 1;
        }
    }

    // warped time of each moment; the clock runs at 1+e, e linear between moments
    let mut w = vec![mo[0].0 as f64; n];
    for k in 1..n {
        let d = (mo[k].0 - mo[k - 1].0) as f64;
        w[k] = w[k - 1] + d * (1.0 + (e[k - 1] + e[k]) / 2.0);
    }
    let warp = |t: i64| -> i64 {
        let k = mo.partition_point(|m| m.0 <= t);
        if k == 0 {
            return t;
        }
        let k = k - 1;
        let u = (t - mo[k].0) as f64;
        let x = if k + 1 < n {
            let d = (mo[k + 1].0 - mo[k].0) as f64;
            w[k] + u * (1.0 + e[k] + (e[k + 1] - e[k]) * u / (2.0 * d))
        } else {
            w[k] + u
        };
        x.round() as i64
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

    fn ratios(ev: &[Ev], a: f64) -> Vec<(i64, f64)> {
        let (x, y) = (ons(ev), ons(&rubato(ev, a)));
        x.windows(2).zip(y.windows(2)).filter(|(p, _)| p[1].0 > p[0].0 + CHORD_US)
            .map(|(p, q)| (p[0].0, (q[1].0 - q[0].0) as f64 / (p[1].0 - p[0].0) as f64)).collect()
    }
    fn mean_in(r: &[(i64, f64)], from: i64, to: i64) -> f64 {
        let v: Vec<f64> = r.iter().filter(|x| x.0 >= from && x.0 < to).map(|x| x.1).collect();
        v.iter().sum::<f64>() / v.len() as f64
    }

    /// Dense passages are played slower and sparse ones faster, within `amount`.
    #[test]
    fn rubato_slows_dense_and_hurries_sparse() {
        let mut v = vec![];
        let mut t = 0;
        while t < 30_000_000 {
            let dense = (10_000_000..20_000_000).contains(&t);
            let step = if dense { 80_000 } else { 250_000 };
            let chord: &[u8] = if dense { &[48, 52, 55, 60, 64, 67] } else { &[72] };
            for &p in chord { v.push(on(t, p)); v.push(off(t + step - 10_000, p)); }
            t += step;
        }
        let a = 0.10;
        let r = ratios(&sorted(v), a);
        assert!(r.iter().all(|x| x.1 > 1.0 - a - 0.002 && x.1 < 1.0 + a + 0.002));
        let (sparse, dense) = (mean_in(&r, 2_000_000, 8_000_000), mean_in(&r, 12_000_000, 18_000_000));
        assert!(dense > 1.0 + a * 0.3, "dense passage slower: {dense}");
        assert!(sparse < 1.0, "sparse passage faster: {sparse}");
    }

    /// A phrase pushes on in its middle and slows into its end; after the rest
    /// the next phrase starts back in tempo.
    #[test]
    fn rubato_phrase_arch_and_a_tempo() {
        let mut v = vec![];
        for ph in 0..2i64 {
            for i in 0..30i64 {
                let t = ph * 7_500_000 + i * 200_000;
                v.push(on(t, 60 + (i % 5) as u8));
                v.push(off(t + 190_000, 60 + (i % 5) as u8));
            }
        }
        let a = 0.10;
        let r = ratios(&sorted(v), a);
        let mid = mean_in(&r, 2_400_000, 3_600_000);
        let end = r.iter().filter(|x| x.0 < 6_000_000).last().unwrap().1;
        assert!(mid < 1.0 - a * 0.2, "middle pushes on: {mid}");
        assert!(end > 1.0 + a * 0.3, "end slows: {end}");
        let first = r.iter().find(|x| x.0 >= 7_500_000).unwrap().1;
        assert!(first < end - a * 0.3, "next phrase back in tempo: {first} vs {end}");
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
