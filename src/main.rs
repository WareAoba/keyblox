#![cfg_attr(windows, windows_subsystem = "windows")]

mod analyze;
mod layout;
mod midi;
mod schedule;
mod ui;
#[cfg(windows)]
mod gui;
#[cfg(windows)]
mod player;

#[cfg(windows)]
fn main() {
    // optional: a .mid dropped onto the exe
    let path = std::env::args().nth(1);
    gui::run(path);
}

/// Non-Windows build: analysis only (used for testing).
#[cfg(not(windows))]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.iter().find(|a| !a.starts_with("--")) else {
        eprintln!("usage: keyblox <file.mid> [--tol MS] [--dump-ons]");
        std::process::exit(1);
    };
    let mut st = analyze::Settings::default();
    let val = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<f64>().ok());
    if let Some(t) = val("--tol") { st.tolerance = Some(t); }
    if let Some(r) = val("--rate") { st.max_rate = Some(r); }
    let bytes = std::fs::read(path).expect("read");
    if args.iter().any(|a| a == "--dump-acts") {
        let a = analyze::analyze(&bytes, &st).unwrap();
        for x in &a.schedule.actions {
            let (d, sc) = match x.act { schedule::Act::Down(s) => (1, s), schedule::Act::Up(s) => (0, s) };
            println!("{} {} {} {}", x.t, d, sc, x.note.is_some() as u8);
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--preview") {
        let out = &args[i + 1];
        let at = val("--at").unwrap_or(0.0);
        let scale = val("--scale").unwrap_or(1.0) as f32;
        let reg = std::fs::read("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc").unwrap();
        let bold = std::fs::read("/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc").unwrap();
        let fonts = ui::Fonts::load(&reg, 1, &bold, 1).unwrap();
        let mut cv = ui::Canvas::new(scale, fonts);
        let mut m = ui::Model::default();
        let a = analyze::analyze(&bytes, &m.settings()).unwrap();
        if !args.iter().any(|a| a == "--empty") {
            m.file_name = Some(path.rsplit('/').next().unwrap().to_string());
            m.summary = Some(a.summary.clone());
            m.set_vis(a.schedule.vis);
        }
        m.pos_us = (at * 1e6) as i64;
        m.state = if at > 0.0 { ui::ST_PLAYING } else { ui::ST_STOPPED };
        if args.iter().any(|a| a == "--hover-play") { m.hover = Some(ui::Wid::Play); }
        if args.iter().any(|a| a == "--novis") { m.vis_on = false; }
        let t0 = std::time::Instant::now();
        let n = 30;
        for k in 0..n { m.pos_us = (at * 1e6) as i64 + k * 16_666; ui::render(&mut cv, &m); }
        eprintln!("render: {:.2} ms/frame ({}x{})", t0.elapsed().as_secs_f64() * 1000.0 / n as f64, cv.pm.width(), cv.pm.height());
        cv.pm.save_png(out).unwrap();
        return;
    }
    if args.iter().any(|a| a == "--why") {
        let a = analyze::analyze(&bytes, &st).unwrap();
        let v = &a.schedule.vis;
        for (i, n) in v.iter().enumerate().filter(|(_, n)| !n.kept) {
            let near: Vec<String> = v.iter().filter(|m| (m.start - n.start).abs() <= 40_000)
                .map(|m| format!("{}{}@{:+.1}", m.pitch, match layout::key_for(m.pitch).unwrap().modifier { layout::Mod::None => "", layout::Mod::Shift => "S", layout::Mod::Ctrl => "C" }, (m.start - n.start) as f64 / 1000.0) + if m.kept { "" } else { "(X)" }).collect();
            println!("#{i} t={:.3}s pitch {}  near: {}", n.start as f64 / 1e6, n.pitch, near.join(" "));
        }
        return;
    }
    if args.iter().any(|a| a == "--dump-ons") {
        let opt = midi::Options { transpose: 0, include_drums: true, pedal: false, pedal_threshold: 64, speed: 1.0 };
        for e in midi::parse(&bytes, &opt).unwrap().events {
            if let midi::Kind::NoteOn(p) = e.kind { println!("{} {}", e.t, p); }
        }
        return;
    }
    match analyze::analyze(&bytes, &st) {
        Ok(a) => { for l in a.report { println!("{l}"); } }
        Err(e) => { eprintln!("{e}"); std::process::exit(1); }
    }
}
