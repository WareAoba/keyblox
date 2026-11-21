//! Windows real-time playback engine (runs on its own thread).
//!
//! * The whole timeline is precomputed; this thread only waits and sends.
//! * Times are absolute from one start Instant (QueryPerformanceCounter):
//!   no drift however long the song.
//! * Wait = 1 ms-resolution sleep until ~1.5 ms before the target, then a
//!   yielding spin (other threads, e.g. Roblox, may run). Priority HIGHEST.
//! * Roblox reads input once per frame, so events due within 1 ms are sent
//!   together in ONE SendInput call (fewer syscalls in dense passages).
//! * If the thread ever wakes late, due events are sent at once, in order.
//! * Keys are sent as hardware scancodes (KEYEVENTF_SCANCODE).

use crate::layout::{SC_LCTRL, SC_LSHIFT, SC_SPACE};
use crate::schedule::{Act, Action, COALESCE_US};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Media::timeBeginPeriod;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcess, QueryFullProcessImageNameW, SetPriorityClass,
    SetThreadPriority, ABOVE_NORMAL_PRIORITY_CLASS, PROCESS_QUERY_LIMITED_INFORMATION, THREAD_PRIORITY_HIGHEST,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    VK_F6, VK_F7,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId};

pub const ST_STOPPED: u8 = 0;
pub const ST_ARMED: u8 = 1; // waiting for Roblox to come to the front
pub const ST_PLAYING: u8 = 2;
pub const ST_PAUSED: u8 = 3;
pub const ST_LOST: u8 = 4; // paused because Roblox lost focus
pub const ST_DONE: u8 = 5;

static STATE: AtomicU8 = AtomicU8::new(ST_STOPPED);
static POS_US: AtomicI64 = AtomicI64::new(0);
static STOP: AtomicBool = AtomicBool::new(false);
static QUIT: AtomicBool = AtomicBool::new(false);
static EXITED: AtomicBool = AtomicBool::new(false);
static BLOCKED: AtomicBool = AtomicBool::new(false);
static ANY_WINDOW: AtomicBool = AtomicBool::new(false);
static TARGET_HWND: AtomicUsize = AtomicUsize::new(0);
static OWN_HWND: AtomicUsize = AtomicUsize::new(0);
static ACTIONS: Mutex<Option<Arc<Vec<Action>>>> = Mutex::new(None);
static ORIGIN_US: AtomicI64 = AtomicI64::new(0); // song start, in us since EPOCH
static SEEK_US: AtomicI64 = AtomicI64::new(-1);
static EPOCH: OnceLock<Instant> = OnceLock::new();
static PLAYBACK: OnceLock<thread::Thread> = OnceLock::new();

/// Wake the playback thread when it is idle (it parks instead of polling).
fn wake() {
    if let Some(t) = PLAYBACK.get() {
        t.unpark();
    }
}

fn since_epoch_us(t: Instant) -> i64 {
    let e = *EPOCH.get_or_init(Instant::now);
    if t >= e { (t - e).as_micros() as i64 } else { -((e - t).as_micros() as i64) }
}

pub fn state() -> u8 {
    STATE.load(SeqCst)
}
/// Smooth song position (for the visualizer).
pub fn position_us() -> i64 {
    if state() == ST_PLAYING {
        since_epoch_us(Instant::now()) - ORIGIN_US.load(SeqCst)
    } else {
        POS_US.load(SeqCst)
    }
}
/// Jump to a song position (any state).
pub fn seek(us: i64) {
    POS_US.store(us.max(0), SeqCst);
    SEEK_US.store(us.max(0), SeqCst);
    wake();
}
pub fn input_blocked() -> bool {
    BLOCKED.load(SeqCst)
}
pub fn set_any_window(v: bool) {
    ANY_WINDOW.store(v, SeqCst);
}
pub fn set_own_window(h: usize) {
    OWN_HWND.store(h, SeqCst);
}

/// Replace the song (stops playback).
pub fn load(actions: Vec<Action>) {
    stop();
    *ACTIONS.lock().unwrap() = Some(Arc::new(actions));
}

pub fn has_song() -> bool {
    ACTIONS.lock().unwrap().is_some()
}

pub fn stop() {
    STATE.store(ST_STOPPED, SeqCst);
    STOP.store(true, SeqCst);
    wake();
}

/// Play/pause (F6 or the GUI button).
pub fn toggle() {
    if !has_song() {
        return;
    }
    match state() {
        ST_PLAYING => STATE.store(ST_PAUSED, SeqCst),
        ST_ARMED => STATE.store(ST_PAUSED, SeqCst),
        _ => {
            if target_in_front() {
                start_now();
            } else {
                STATE.store(ST_ARMED, SeqCst);
            }
        }
    }
}

pub fn shutdown() {
    QUIT.store(true, SeqCst);
    wake();
    for _ in 0..200 {
        if EXITED.load(SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn start_now() {
    TARGET_HWND.store(foreground(), SeqCst);
    if state() == ST_DONE {
        STOP.store(true, SeqCst);
    }
    STATE.store(ST_PLAYING, SeqCst);
    wake();
}

fn foreground() -> usize {
    unsafe { GetForegroundWindow() as usize }
}

fn foreground_title() -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(GetForegroundWindow(), buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Executable name of the foreground window's process, e.g. "RobloxPlayerBeta.exe".
fn foreground_exe() -> String {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(GetForegroundWindow(), &mut pid);
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        if ok == 0 {
            return String::new();
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        full.rsplit('\\').next().unwrap_or("").to_string()
    }
}

/// Is the window we should type into in front?
fn target_in_front() -> bool {
    let fg = foreground();
    if fg == 0 || fg == OWN_HWND.load(SeqCst) {
        return false; // never type into our own window
    }
    if ANY_WINDOW.load(SeqCst) {
        return true;
    }
    foreground_title() == "Roblox" || foreground_exe().to_ascii_lowercase().starts_with("robloxplayer")
}

fn key_input(scan: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: scan,
                dwFlags: KEYEVENTF_SCANCODE | if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(batch: &[INPUT]) -> bool {
    if batch.is_empty() {
        return true;
    }
    let n = unsafe { SendInput(batch.len() as u32, batch.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    n as usize == batch.len()
}

fn pressed(vk: u16) -> bool {
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

/// Sleep/spin until `target`. Returns false if playback was interrupted.
#[inline]
fn wait_until(target: Instant) -> bool {
    loop {
        let now = Instant::now();
        if now >= target {
            return true;
        }
        let rem = target - now;
        if state() != ST_PLAYING || STOP.load(SeqCst) || QUIT.load(SeqCst) || SEEK_US.load(SeqCst) >= 0 {
            return false;
        }
        if rem > Duration::from_micros(2000) {
            thread::sleep(Duration::from_micros(1000).min(rem - Duration::from_micros(1500)));
        } else {
            thread::yield_now();
        }
    }
}

fn release_all(physical: &mut [bool; 256]) {
    let ups: Vec<INPUT> = (0..256).filter(|&s| physical[s]).map(|s| key_input(s as u16, true)).collect();
    send(&ups);
    physical.iter_mut().for_each(|p| *p = false);
}

fn playback() {
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
    let mut acts: Arc<Vec<Action>> = Arc::new(vec![]);
    let mut logical = [false; 256]; // what the timeline says is down
    let mut physical = [false; 256]; // what we actually hold down
    let mut idx = 0usize;
    let mut pos = Duration::ZERO;
    let mut fresh = true;
    let mut batch: Vec<INPUT> = Vec::with_capacity(128);

    loop {
        if QUIT.load(SeqCst) {
            release_all(&mut physical);
            EXITED.store(true, SeqCst);
            return;
        }
        if STOP.swap(false, SeqCst) {
            release_all(&mut physical);
            logical = [false; 256];
            idx = 0;
            pos = Duration::ZERO;
            POS_US.store(0, SeqCst);
            fresh = true;
        }
        let sk = SEEK_US.swap(-1, SeqCst);
        if sk >= 0 {
            release_all(&mut physical);
            if fresh {
                if let Some(a) = ACTIONS.lock().unwrap().clone() {
                    acts = a;
                    fresh = false;
                }
            }
            idx = acts.partition_point(|a| a.t < sk);
            // modifier / pedal state the timeline expects at that point
            logical = [false; 256];
            for a in &acts[..idx] {
                match a.act {
                    Act::Down(s) => logical[s as usize] = true,
                    Act::Up(s) => logical[s as usize] = false,
                }
            }
            pos = Duration::from_micros(sk as u64);
            POS_US.store(sk, SeqCst);
        }
        if state() != ST_PLAYING {
            // every control call (start, stop, seek, quit) unparks us; the
            // timeout is only a safety net
            thread::park_timeout(Duration::from_millis(100));
            continue;
        }
        if fresh {
            match ACTIONS.lock().unwrap().clone() {
                Some(a) => acts = a,
                None => {
                    STATE.store(ST_STOPPED, SeqCst);
                    continue;
                }
            }
            fresh = false;
        }

        // (re)start: put modifiers & pedal back the way the timeline expects
        let restore: Vec<INPUT> = [SC_LSHIFT, SC_LCTRL, SC_SPACE]
            .iter()
            .filter(|&&s| logical[s as usize])
            .map(|&s| {
                physical[s as usize] = true;
                key_input(s, false)
            })
            .collect();
        send(&restore);
        let hwnd = TARGET_HWND.load(SeqCst);
        let any = ANY_WINDOW.load(SeqCst);
        let origin = Instant::now() - pos;
        ORIGIN_US.store(since_epoch_us(origin), SeqCst);

        while idx < acts.len() {
            let t = acts[idx].t.max(0) as u64;
            if !wait_until(origin + Duration::from_micros(t)) {
                break;
            }
            if state() != ST_PLAYING || STOP.load(SeqCst) || QUIT.load(SeqCst) || SEEK_US.load(SeqCst) >= 0 {
                break;
            }
            if !any && foreground() != hwnd {
                STATE.store(ST_LOST, SeqCst);
                break;
            }
            batch.clear();
            let now_us = (Instant::now() - origin).as_micros() as i64;
            while idx < acts.len() && acts[idx].t <= now_us + COALESCE_US {
                match acts[idx].act {
                    Act::Down(s) => {
                        logical[s as usize] = true;
                        physical[s as usize] = true;
                        batch.push(key_input(s, false));
                    }
                    Act::Up(s) => {
                        logical[s as usize] = false;
                        if physical[s as usize] {
                            physical[s as usize] = false;
                            batch.push(key_input(s, true));
                        }
                    }
                }
                idx += 1;
            }
            if !send(&batch) {
                BLOCKED.store(true, SeqCst);
            }
            POS_US.store(now_us, SeqCst);
        }

        if idx >= acts.len() {
            release_all(&mut physical);
            logical = [false; 256];
            idx = 0;
            pos = Duration::ZERO;
            fresh = true;
            if state() == ST_PLAYING {
                STATE.store(ST_DONE, SeqCst);
            }
            continue;
        }
        let sk = SEEK_US.load(SeqCst);
        if sk < 0 {
            pos = Instant::now() - origin;
            POS_US.store(pos.as_micros() as i64, SeqCst);
        }
        release_all(&mut physical);
    }
}

/// Global hotkeys (work while Roblox is focused) + arming.
fn hotkeys() {
    let keys = [VK_F6, VK_F7];
    let mut prev = [false; 2];
    let mut armed_since: Option<Instant> = None;
    while !QUIT.load(SeqCst) {
        for (i, &vk) in keys.iter().enumerate() {
            let now = pressed(vk);
            if now && !prev[i] {
                if vk == VK_F6 {
                    toggle();
                } else {
                    stop();
                }
            }
            prev[i] = now;
        }
        if state() == ST_ARMED {
            if target_in_front() {
                // give the click that focused Roblox a moment to finish
                let since = *armed_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_millis(400) && state() == ST_ARMED {
                    start_now();
                    armed_since = None;
                }
            } else {
                armed_since = None;
            }
        } else {
            armed_since = None;
        }
        thread::sleep(Duration::from_millis(3));
    }
}

pub fn init() {
    EPOCH.get_or_init(Instant::now);
    unsafe {
        timeBeginPeriod(1);
        SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
    }
    let t = thread::spawn(playback);
    let _ = PLAYBACK.set(t.thread().clone());
    thread::spawn(hotkeys);
}
