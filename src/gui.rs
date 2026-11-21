//! Windows shell around the software-rendered UI (ui.rs): window, DPI, mouse,
//! file dialog, drag & drop, background analysis. No extra runtime, no GPU.

use crate::analyze;
use crate::player;
use crate::ui::{self, Canvas, Fonts, Model, Wid};
use std::cell::RefCell;
use std::ptr::{null, null_mut};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, SetDIBitsToDevice, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST, OPENFILENAMEW};
use windows_sys::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows_sys::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_ANALYZED: u32 = WM_APP + 1;
const WM_MOUSELEAVE_: u32 = 0x02A3;
const STYLE: u32 = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;

struct App {
    hwnd: HWND,
    model: Model,
    cv: Canvas,
    file: Option<String>,
    gen: u64,
    tracking: bool,
    /// what the last painted frame showed: (state, ui::frame_key, analyzing, hover)
    last_frame: (u8, i64, bool, Option<Wid>),
    dirty: bool,
}

struct Done {
    gen: u64,
    result: Result<analyze::Analysis, String>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}
static DONE: Mutex<Option<Done>> = Mutex::new(None);

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Run `f` on the app state if it is not already borrowed (re-entrancy safe).
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| match a.try_borrow_mut() {
        Ok(mut g) => g.as_mut().map(f),
        Err(_) => None,
    })
}

fn load_fonts() -> Fonts {
    let dir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    let read = |f: &str| std::fs::read(format!("{dir}\\Fonts\\{f}")).ok();
    let reg = read("malgun.ttf").or_else(|| read("segoeui.ttf")).expect("no usable font");
    let bold = read("malgunbd.ttf").or_else(|| read("segoeuib.ttf")).unwrap_or_else(|| reg.clone());
    Fonts::load(&reg, 0, &bold, 0).expect("font")
}

fn scale_of(hwnd: HWND) -> f32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
}

fn invalidate(hwnd: HWND) {
    unsafe {
        InvalidateRect(hwnd, null(), 0);
    }
}

/// Start (re)analysis of the current file on a worker thread.
fn analyze_async(app: &mut App) {
    let Some(path) = app.file.clone() else { return };
    app.gen += 1;
    let gen = app.gen;
    app.model.analyzing = true;
    app.model.error = None;
    app.dirty = true;
    player::stop();
    let st = app.model.settings();
    let hwnd = app.hwnd as usize;
    std::thread::spawn(move || {
        let result = std::fs::read(&path)
            .map_err(|e| format!("파일을 열 수 없습니다: {e}"))
            .and_then(|b| analyze::analyze(&b, &st));
        *DONE.lock().unwrap() = Some(Done { gen, result });
        unsafe {
            PostMessageW(hwnd as HWND, WM_ANALYZED, 0, 0);
        }
    });
}

fn open_file(app: &mut App, path: String) {
    app.model.file_name = Some(path.rsplit(['\\', '/']).next().unwrap_or(&path).to_string());
    app.model.summary = None;
    app.model.set_vis(vec![]);
    app.file = Some(path);
    analyze_async(app);
}

fn pick_file(owner: HWND) -> Option<String> {
    let mut buf = [0u16; 1024];
    let filter: Vec<u16> = "MIDI 파일 (*.mid;*.midi)\0*.mid;*.midi\0모든 파일\0*.*\0\0".encode_utf16().collect();
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST;
    if unsafe { GetOpenFileNameW(&mut ofn) } != 0 {
        let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
        Some(String::from_utf16_lossy(&buf[..n]))
    } else {
        None
    }
}

fn mouse_xy(app: &App, lp: LPARAM) -> (f32, f32) {
    let x = (lp & 0xFFFF) as i16 as f32;
    let y = ((lp >> 16) & 0xFFFF) as i16 as f32;
    (x / app.cv.s, y / app.cv.s)
}

fn seek_to(app: &mut App, x: f32) {
    let len = app.model.length_us();
    if len > 0 && !app.model.analyzing {
        let us = (ui::seek_fraction(x) * len as f64) as i64;
        player::seek(us);
        app.model.pos_us = us;
        app.dirty = true;
    }
}

fn click(hwnd: HWND, wid: Wid) {
    match wid {
        Wid::Open => {
            // modal dialog: must not hold the app borrow while it runs
            if let Some(p) = pick_file(hwnd) {
                with_app(|a| open_file(a, p));
            }
        }
        Wid::Play => {
            with_app(|a| {
                if a.model.summary.as_ref().map(|s| s.ok).unwrap_or(false) && !a.model.analyzing {
                    player::toggle();
                }
            });
        }
        Wid::Stop => player::stop(),
        Wid::Seek => {}
        w => {
            with_app(|a| {
                if a.model.click_setting(w) {
                    analyze_async(a);
                }
                player::set_any_window(!a.model.roblox_only);
                a.dirty = true;
            });
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            with_app(|a| {
                ui::render(&mut a.cv, &a.model);
                let (wd, ht) = (a.cv.pm.width() as i32, a.cv.pm.height() as i32);
                let mut bi: BITMAPINFO = std::mem::zeroed();
                bi.bmiHeader = BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: wd,
                    biHeight: -ht, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB as u32,
                    ..std::mem::zeroed()
                };
                SetDIBitsToDevice(
                    hdc, 0, 0, wd as u32, ht as u32, 0, 0, 0, ht as u32,
                    a.cv.pm.data().as_ptr() as *const _, &bi, DIB_RGB_COLORS,
                );
                a.last_frame = (a.model.state, ui::frame_key(&a.model, a.cv.s), a.model.analyzing, a.model.hover);
                a.dirty = false;
            });
            EndPaint(hwnd, &ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_TIMER => {
            with_app(|a| {
                let m = &mut a.model;
                m.state = player::state();
                m.pos_us = player::position_us().min(m.length_us().max(0));
                m.blocked = player::input_blocked();
                if IsIconic(hwnd) != 0 {
                    return; // nothing to show; restoring repaints the whole window
                }
                let key = ui::frame_key(m, a.cv.s);
                let (st, _, an, hv) = a.last_frame;
                if a.dirty || (st, an, hv) != (m.state, m.analyzing, m.hover) {
                    invalidate(hwnd);
                } else if key != a.last_frame.1 {
                    // only the visualizer / transport moved: repaint just that part
                    let r = ui::dynamic_rect();
                    let s = a.cv.s;
                    let rc = RECT {
                        left: (r.x * s).floor() as i32,
                        top: (r.y * s).floor() as i32,
                        right: ((r.x + r.w) * s).ceil() as i32,
                        bottom: ((r.y + r.h) * s).ceil() as i32,
                    };
                    InvalidateRect(hwnd, &rc, 0);
                }
            });
            0
        }
        WM_MOUSEMOVE => {
            with_app(|a| {
                if !a.tracking {
                    let mut t = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    TrackMouseEvent(&mut t);
                    a.tracking = true;
                }
                let (x, y) = mouse_xy(a, lp);
                let h = ui::hit(x, y);
                if h != a.model.hover {
                    a.model.hover = h;
                    a.dirty = true;
                }
                if a.model.pressed == Some(Wid::Seek) {
                    seek_to(a, x);
                }
            });
            0
        }
        WM_MOUSELEAVE_ => {
            with_app(|a| {
                a.tracking = false;
                a.model.hover = None;
                a.dirty = true;
            });
            0
        }
        WM_LBUTTONDOWN => {
            with_app(|a| {
                let (x, y) = mouse_xy(a, lp);
                a.model.pressed = ui::hit(x, y);
                if a.model.pressed == Some(Wid::Seek) {
                    SetCapture(hwnd);
                    seek_to(a, x);
                }
                a.dirty = true;
            });
            0
        }
        WM_LBUTTONUP => {
            let target = with_app(|a| {
                let (x, y) = mouse_xy(a, lp);
                let p = a.model.pressed.take();
                a.dirty = true;
                if p == Some(Wid::Seek) {
                    ReleaseCapture();
                    None
                } else if p.is_some() && p == ui::hit(x, y) {
                    p
                } else {
                    None
                }
            })
            .flatten();
            if let Some(wid) = target {
                click(hwnd, wid);
            }
            0
        }
        WM_SETCURSOR => {
            let over = with_app(|a| a.model.hover.is_some()).unwrap_or(false);
            if (lp & 0xFFFF) as u32 == HTCLIENT {
                SetCursor(LoadCursorW(null_mut(), if over { IDC_HAND } else { IDC_ARROW }));
                return 1;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_DROPFILES => {
            let drop = wp as HDROP;
            let mut buf = [0u16; 1024];
            let n = DragQueryFileW(drop, 0, buf.as_mut_ptr(), buf.len() as u32) as usize;
            DragFinish(drop);
            if n > 0 {
                let p = String::from_utf16_lossy(&buf[..n]);
                with_app(|a| open_file(a, p));
            }
            0
        }
        WM_ANALYZED => {
            let done = DONE.lock().unwrap().take();
            if let Some(done) = done {
                with_app(|a| {
                    if done.gen != a.gen {
                        return; // a newer analysis is running
                    }
                    a.model.analyzing = false;
                    match done.result {
                        Ok(an) => {
                            a.model.summary = Some(an.summary.clone());
                            if an.ok {
                                player::load(an.schedule.actions);
                            }
                            a.model.set_vis(an.schedule.vis);
                        }
                        Err(e) => {
                            a.model.error = Some(e);
                            a.model.summary = None;
                        }
                    }
                    a.dirty = true;
                });
                invalidate(hwnd);
            }
            0
        }
        0x02E0 /* WM_DPICHANGED */ => {
            let r = &*(lp as *const RECT);
            with_app(|a| {
                a.cv.set_scale(((wp & 0xFFFF) as f32) / 96.0);
                a.dirty = true;
            });
            SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            0
        }
        WM_DESTROY => {
            player::shutdown();
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn window_size(scale: f32, dpi: u32) -> (i32, i32) {
    let mut rc = RECT { left: 0, top: 0, right: (ui::W * scale).round() as i32, bottom: (ui::H * scale).round() as i32 };
    unsafe {
        AdjustWindowRectExForDpi(&mut rc, STYLE, 0, 0, dpi);
    }
    (rc.right - rc.left, rc.bottom - rc.top)
}

pub fn run(initial: Option<String>) {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    player::init();
    let fonts = load_fonts();
    unsafe {
        let hinst = GetModuleHandleW(null());
        let class = w("keyblox");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: LoadIconW(hinst, 1 as _),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: null_mut(),
        };
        RegisterClassExW(&wc);
        let title = w("keyblox");
        let hwnd = CreateWindowExW(
            0, class.as_ptr(), title.as_ptr(), STYLE, CW_USEDEFAULT, CW_USEDEFAULT, 800, 600,
            null_mut(), null_mut(), hinst, null(),
        );
        // dark title bar (Windows 10 2004+ / 11; ignored elsewhere)
        let dark: i32 = 1;
        DwmSetWindowAttribute(hwnd, 20, &dark as *const _ as *const _, 4);

        let scale = scale_of(hwnd);
        let (ww, wh) = window_size(scale, GetDpiForWindow(hwnd).max(96));
        SetWindowPos(hwnd, null_mut(), 0, 0, ww, wh, SWP_NOMOVE | SWP_NOZORDER);

        player::set_own_window(hwnd as usize);
        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                hwnd,
                model: Model::default(),
                cv: Canvas::new(scale, fonts),
                file: None,
                gen: 0,
                tracking: false,
                last_frame: (255, -1, false, None),
                dirty: true,
            })
        });
        DragAcceptFiles(hwnd, 1);
        SetTimer(hwnd, 1, 15, None);
        ShowWindow(hwnd, SW_SHOW);
        if let Some(p) = initial {
            with_app(|a| open_file(a, p));
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
