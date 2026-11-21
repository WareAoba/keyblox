//! MIDI file -> flat, time-sorted list of note/pedal events in microseconds.
//! Every tempo change in any track is honoured; tick->time conversion is
//! done with exact integer arithmetic so long songs never drift.

use midly::{MetaMessage, MidiMessage, Smf, Timing, TrackEventKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    // order matters: at the same instant, offs run before pedal, pedal before ons
    NoteOff(u8),
    Pedal(bool),
    NoteOn(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ev {
    pub t: i64, // microseconds from song start
    pub kind: Kind,
}

pub struct Options {
    pub transpose: i32,
    pub include_drums: bool,
    pub pedal: bool,
    pub pedal_threshold: u8,
    pub speed: f64,
}

pub struct Parsed {
    pub events: Vec<Ev>,
    pub tempo_changes: usize,
    pub notes: usize,
    pub folded: usize,
    pub pedal_changes: usize,
    pub length_us: i64,
    /// Original BPM when every note plays under one tempo (None: tempo changes, or timecode timing)
    pub bpm: Option<f64>,
}

enum Raw {
    Tempo(u32),
    On(u8, u8),  // channel, key
    Off(u8, u8), // channel, key
    Sustain(u8, u8), // channel, value
}

pub fn parse(bytes: &[u8], opt: &Options) -> Result<Parsed, String> {
    let smf = Smf::parse(bytes).map_err(|e| format!("MIDI 파싱 실패: {e}"))?;

    // collect every event with absolute tick
    let mut raw: Vec<(u64, u32, Raw)> = Vec::new(); // (tick, seq, ev)
    let mut seq = 0u32;
    for track in &smf.tracks {
        let mut tick = 0u64;
        for e in track {
            tick += e.delta.as_int() as u64;
            let r = match e.kind {
                TrackEventKind::Meta(MetaMessage::Tempo(t)) => Some(Raw::Tempo(t.as_int())),
                TrackEventKind::Midi { channel, message } => {
                    let ch = channel.as_int();
                    match message {
                        MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => Some(Raw::On(ch, key.as_int())),
                        MidiMessage::NoteOn { key, .. } | MidiMessage::NoteOff { key, .. } => {
                            Some(Raw::Off(ch, key.as_int()))
                        }
                        MidiMessage::Controller { controller, value } if controller.as_int() == 64 => {
                            Some(Raw::Sustain(ch, value.as_int()))
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some(r) = r {
                raw.push((tick, seq, r));
                seq += 1;
            }
        }
    }
    // tempo events first at equal ticks (they don't move that tick, but keep it tidy)
    raw.sort_by_key(|(tick, s, r)| (*tick, !matches!(r, Raw::Tempo(_)), *s));

    // tick -> microseconds
    // Metrical: us = sum(dtick * tempo) / tpq      (exact, u128)
    // Timecode: us = tick * 1e6 / (fps * subframes)
    let (tpq, tc_div) = match smf.header.timing {
        Timing::Metrical(t) => (t.as_int() as u128, None),
        Timing::Timecode(fps, sub) => (0, Some(fps.as_f32() as f64 * sub as f64)),
    };
    if tpq == 0 && tc_div.is_none() {
        return Err("잘못된 MIDI 헤더 (division = 0)".into());
    }
    let mut tempo: u128 = 500_000; // default 120 BPM
    let mut acc: u128 = 0; // sum(dtick*tempo)
    let mut last_tick = 0u64;
    let speed = if opt.speed > 0.0 { opt.speed } else { 1.0 };

    let mut events = Vec::new();
    let mut tempo_changes = 0;
    let mut notes = 0;
    let mut folded = 0;
    let mut pedal_changes = 0;
    let mut chan_pedal = [false; 16];
    let mut pedal_any = false;
    let mut heard_tempo: Option<u128> = None; // tempo in effect at note-ons
    let mut mixed_tempo = tc_div.is_some();

    for (tick, _, r) in raw {
        let us = if let Some(div) = tc_div {
            tick as f64 * 1e6 / div
        } else {
            acc += (tick - last_tick) as u128 * tempo;
            last_tick = tick;
            (acc / tpq) as f64
        };
        let t = (us / speed).round() as i64;
        match r {
            Raw::Tempo(tp) => {
                tempo = tp.max(1) as u128;
                tempo_changes += 1;
            }
            Raw::On(ch, k) | Raw::Off(ch, k) if ch == 9 && !opt.include_drums => {
                let _ = k;
            }
            Raw::On(_, k) => {
                let p = k as i32 + opt.transpose;
                let f = crate::layout::fold(p);
                if f as i32 != p {
                    folded += 1;
                }
                notes += 1;
                mixed_tempo |= heard_tempo.is_some_and(|h| h != tempo);
                heard_tempo = Some(tempo);
                events.push(Ev { t, kind: Kind::NoteOn(f) });
            }
            Raw::Off(_, k) => {
                let f = crate::layout::fold(k as i32 + opt.transpose);
                events.push(Ev { t, kind: Kind::NoteOff(f) });
            }
            Raw::Sustain(ch, v) => {
                if !opt.pedal {
                    continue;
                }
                chan_pedal[ch as usize] = v >= opt.pedal_threshold;
                let any = chan_pedal.iter().any(|&b| b);
                if any != pedal_any {
                    pedal_any = any;
                    pedal_changes += 1;
                    events.push(Ev { t, kind: Kind::Pedal(any) });
                }
            }
        }
    }
    // stable: keeps file order for equal (t, kind-class)
    events.sort_by_key(|e| (e.t, class(e.kind)));
    let length_us = events.last().map(|e| e.t).unwrap_or(0);
    let bpm = heard_tempo.filter(|_| !mixed_tempo).map(|t| 60e6 / t as f64);
    Ok(Parsed { events, tempo_changes, notes, folded, pedal_changes, length_us, bpm })
}

fn class(k: Kind) -> u8 {
    match k {
        Kind::NoteOff(_) => 0,
        Kind::Pedal(_) => 1,
        Kind::NoteOn(_) => 2,
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use midly::num::{u15, u24, u28, u4, u7};
    use midly::{Format, Header, MidiMessage, TrackEvent};

    pub fn opts() -> Options {
        Options { transpose: 0, include_drums: false, pedal: true, pedal_threshold: 64, speed: 1.0 }
    }

    fn ev(delta: u32, kind: TrackEventKind<'static>) -> TrackEvent<'static> {
        TrackEvent { delta: u28::new(delta), kind }
    }
    fn on(k: u8) -> TrackEventKind<'static> {
        TrackEventKind::Midi { channel: u4::new(0), message: MidiMessage::NoteOn { key: u7::new(k), vel: u7::new(90) } }
    }
    fn off(k: u8) -> TrackEventKind<'static> {
        TrackEventKind::Midi { channel: u4::new(0), message: MidiMessage::NoteOff { key: u7::new(k), vel: u7::new(0) } }
    }
    fn tempo(us: u32) -> TrackEventKind<'static> {
        TrackEventKind::Meta(MetaMessage::Tempo(u24::new(us)))
    }
    fn cc64(v: u8) -> TrackEventKind<'static> {
        TrackEventKind::Midi { channel: u4::new(0), message: MidiMessage::Controller { controller: u7::new(64), value: u7::new(v) } }
    }

    pub fn write(tracks: Vec<Vec<TrackEvent<'static>>>) -> Vec<u8> {
        let fmt = if tracks.len() > 1 { Format::Parallel } else { Format::SingleTrack };
        let smf = Smf { header: Header::new(fmt, Timing::Metrical(u15::new(480))), tracks };
        let mut out = Vec::new();
        smf.write_std(&mut out).unwrap();
        out
    }

    #[test]
    fn tempo_changes_mid_song_in_separate_track() {
        // conductor track: 120 BPM, after 2 beats -> 60 BPM, after 2 more beats -> 240 BPM
        let conductor = vec![ev(0, tempo(500_000)), ev(960, tempo(1_000_000)), ev(960, tempo(250_000))];
        // notes every beat (480 ticks)
        let mut notes = vec![];
        for i in 0..6 {
            notes.push(ev(if i == 0 { 0 } else { 0 }, on(60)));
            notes.push(ev(480, off(60)));
        }
        let bytes = write(vec![conductor, notes]);
        let p = parse(&bytes, &opts()).unwrap();
        let ons: Vec<i64> = p.events.iter().filter(|e| matches!(e.kind, Kind::NoteOn(_))).map(|e| e.t).collect();
        // beats 0,1 at 0.5s each; beats 2,3 at 1.0s each; beats 4,5 at 0.25s each
        assert_eq!(ons, vec![0, 500_000, 1_000_000, 2_000_000, 3_000_000, 3_250_000]);
        assert_eq!(p.tempo_changes, 3);
    }

    #[test]
    fn speed_and_pedal_and_fold() {
        let t = vec![
            ev(0, cc64(127)), ev(0, on(10)), ev(480, off(10)), ev(0, cc64(0)), ev(0, cc64(30)),
        ];
        let mut o = opts();
        o.speed = 2.0;
        let p = parse(&write(vec![t]), &o).unwrap();
        assert_eq!(p.folded, 1);
        let kinds: Vec<(i64, Kind)> = p.events.iter().map(|e| (e.t, e.kind)).collect();
        assert_eq!(kinds, vec![
            (0, Kind::Pedal(true)), (0, Kind::NoteOn(22)),
            (250_000, Kind::NoteOff(22)), (250_000, Kind::Pedal(false)),
        ]);
    }

    #[test]
    fn single_tempo_bpm() {
        let notes = || vec![ev(0, on(60)), ev(480, off(60)), ev(0, on(62)), ev(480, off(62))];
        // no tempo event -> MIDI default 120 BPM
        assert_eq!(parse(&write(vec![notes()]), &opts()).unwrap().bpm, Some(120.0));
        // the same tempo repeated in several tracks is still one tempo
        let c1 = vec![ev(0, tempo(400_000))];
        let mut t2 = vec![ev(0, tempo(400_000))];
        t2.extend(notes());
        assert_eq!(parse(&write(vec![c1, t2]), &opts()).unwrap().bpm, Some(150.0));
        // a change after the last note doesn't affect what is heard
        let mut t = vec![ev(0, tempo(500_000))];
        t.extend(notes());
        t.push(ev(480, tempo(1_000_000)));
        assert_eq!(parse(&write(vec![t]), &opts()).unwrap().bpm, Some(120.0));
        // a real change between notes -> not a single-tempo song
        let t = vec![ev(0, tempo(500_000)), ev(0, on(60)), ev(480, off(60)), ev(0, tempo(250_000)), ev(0, on(62)), ev(480, off(62))];
        assert_eq!(parse(&write(vec![t]), &opts()).unwrap().bpm, None);
    }
}
