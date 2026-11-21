//! Shared "load + schedule + report" used by both the GUI and the CLI.

use crate::{midi, schedule};

#[derive(Clone, Copy)]
pub struct Settings {
    pub fps: f64,
    pub speed: f64,
    pub transpose: i32,
    pub pedal: bool,
    pub drums: bool,
    /// optional overrides in ms
    pub dwell: Option<f64>,
    pub hold: Option<f64>,
    pub gap: Option<f64>,
    pub tolerance: Option<f64>,
    /// strikes per second cap (None = default, 0 = unlimited)
    pub max_rate: Option<f64>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { fps: 60.0, speed: 1.0, transpose: 0, pedal: true, drums: false, dwell: None, hold: None, gap: None, tolerance: None, max_rate: None }
    }
}

#[allow(dead_code)]
#[derive(Clone, Default)]
pub struct Summary {
    pub notes: usize,
    pub kept: usize,
    pub dropped: usize,
    pub tempo_changes: usize,
    pub pedal_changes: usize,
    pub length_us: i64,
    pub max_late_us: i64,
    pub ok: bool,
}

#[allow(dead_code)]
pub struct Analysis {
    pub schedule: schedule::Schedule,
    pub report: Vec<String>,
    pub length_us: i64,
    pub ok: bool,
    pub summary: Summary,
}

fn mmss(us: i64) -> String {
    let s = us.max(0) as f64 / 1e6;
    format!("{}:{:04.1}", (s / 60.0) as u64, s % 60.0)
}

pub fn analyze(bytes: &[u8], st: &Settings) -> Result<Analysis, String> {
    let opt = midi::Options {
        transpose: st.transpose,
        include_drums: st.drums,
        pedal: st.pedal,
        pedal_threshold: 64,
        speed: st.speed.clamp(0.1, 10.0),
    };
    let parsed = midi::parse(bytes, &opt)?;
    let base = schedule::Params::from_fps(st.fps.clamp(10.0, 1000.0));
    let ms = |v: Option<f64>, d: i64| v.map(|x| (x * 1000.0).round() as i64).unwrap_or(d).max(0);
    let p = schedule::Params {
        dwell: ms(st.dwell, base.dwell),
        hold: ms(st.hold, base.hold),
        gap: ms(st.gap, base.gap),
        tolerance: ms(st.tolerance, base.tolerance),
        max_rate: st.max_rate.unwrap_or(base.max_rate).max(0.0),
    };
    let t0 = std::time::Instant::now();
    let s = schedule::build(&parsed.events, p);
    let build_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let verdict = schedule::verify(&s.actions, p);

    let mut r = vec![];
    r.push(format!("길이: {}   (속도 x{})", mmss(parsed.length_us), opt.speed));
    r.push(format!("음표: {}개   (88키 밖이라 옥타브 이동 {}개)", parsed.notes, parsed.folded));
    r.push(format!("템포 메타 이벤트: {}개 (모두 반영)", parsed.tempo_changes));
    r.push(if st.pedal { format!("페달: {}회 변화 -> 스페이스", parsed.pedal_changes) } else { "페달: 끔".into() });
    r.push(format!(
        "규칙: Shift/Ctrl 전환 간격 {:.1}ms ({}fps), 허용 지연 {:.1}ms, 초당 최대 타건 {}",
        p.dwell as f64 / 1e3, st.fps, p.tolerance as f64 / 1e3,
        if p.max_rate > 0.0 { format!("{:.0}", p.max_rate) } else { "제한 없음".into() }
    ));
    let kept = 100.0 * s.strikes as f64 / (s.strikes + s.dropped).max(1) as f64;
    r.push(format!(
        "연주하는 음 {}개 / 뺀 음 {}개 (유지율 {:.1}%)   (계산 {:.0}ms)",
        s.strikes, s.dropped, kept, build_ms
    ));
    r.push(format!(
        "원래 시각보다 늦은 타건 {}회 (최대 {:.1}ms, 화음 굴림) - 누적 지연 없음",
        s.delayed,
        s.max_late as f64 / 1e3
    ));
    r.push(format!("겹친 같은 음 합침 {}개, 같은 키(t/T 등) 공유로 짧아진 음 {}개", s.merged, s.cut_by_clash));
    let ok = verdict.is_ok();
    r.push(match verdict {
        Ok(()) => "검증: 통과 (모든 타건이 올바른 Shift/Ctrl 상태로 처리됨)".into(),
        Err(e) => format!("검증: 실패 - {e}"),
    });
    let summary = Summary {
        notes: parsed.notes,
        kept: s.strikes,
        dropped: s.dropped,
        tempo_changes: parsed.tempo_changes,
        pedal_changes: parsed.pedal_changes,
        length_us: parsed.length_us,
        max_late_us: s.max_late,
        ok,
    };
    Ok(Analysis { schedule: s, report: r, length_us: parsed.length_us, ok, summary })
}
