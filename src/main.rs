#![windows_subsystem = "windows"]
// FloatIsland: a Dynamic-Island-style pill pinned to the top of the primary monitor.
// One layered Win32 window drawn with Direct2D; repaints only on data ticks (1 Hz) and while animating.
mod data;
mod install;

use data::*;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateMutexW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFY_ICON_MESSAGE, NOTIFYICONDATAW, QUNS_BUSY,
    QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

// Window canvas in DIPs; the pill is drawn inside it, transparent pixels click through.
const CANVAS_W: f32 = 480.0;
const CANVAS_H: f32 = 350.0;
const TOP: f32 = 6.0;
const EDGE: f32 = 12.0; // bottom position: gap (DIP) to the screen's right edge and the taskbar
const FLASH_SECS: u64 = 5;
const ANIM_TIMER: usize = 1;
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_MOUSEWHEEL: u32 = 0x020A;

// The pill grows with its content up to these limits.
const MAX_W: f32 = 460.0;
const MAX_H: f32 = 340.0;
const EXPANDED_W: f32 = 440.0;
// Expanded section heights (draw() and expanded_h() must agree).
const PAD_TOP: f32 = 16.0;
const PAD_BOTTOM: f32 = 6.0;
const MEDIA_H: f32 = 50.0;
const NOTIF_H: f32 = 50.0;
const METER_H: f32 = 38.0;
const SESSION_H: f32 = 20.0;
const BRIGHT_H: f32 = 24.0;
const PAGE_HDR: f32 = 28.0;
const BRIGHT_STEP: i32 = 5; // per mouse-wheel notch
const MAX_SESSIONS: usize = 3;

const ORANGE: u32 = 0xD97757;
const GRAY: u32 = 0x9A9A9A;
const DIM: u32 = 0x3A3A3A;
const WHITE: u32 = 0xFFFFFF;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Collapsed,
    Flash,
    Expanded,
}

#[derive(Clone, Copy)]
enum Hit {
    Media(fn() -> Cmd),
    NotifPrev,
    NotifNext,
    NotifDismiss,
    PagePrev,
    PageNext,
}

struct App {
    hwnd: HWND,
    state: Arc<Mutex<State>>,
    tx: mpsc::Sender<Cmd>,
    rt: ID2D1DCRenderTarget,
    dw: IDWriteFactory,
    text: IDWriteTextFormat,
    small: IDWriteTextFormat,
    small_center: IDWriteTextFormat,
    icons: IDWriteTextFormat,
    memdc: HDC,
    px: SIZE,
    scale: f32,
    cur: [f32; 3], // animated (width, height, radius)
    hover: bool,
    buttons: Vec<(D2D_RECT_F, Hit)>,
    notif_idx: usize,
    seen_flash: Option<Instant>,
    shown: Mode,  // mode whose content is on screen; content hides only while switching modes
    hidden: bool, // a fullscreen app is in front
    sliders: Vec<(D2D_RECT_F, usize)>, // brightness track (row-high hit area), display index
    drag: Option<(usize, u32)>,        // brightness being dragged: (display, pct), sent on release
    page: usize,                       // pager: 0 = Claude quota, 1 = brightness
    slot: f32,                         // animated 0..1: indicator icon presence (text shift + icon fade)
    last_icon: Option<(&'static str, u32)>, // kept so the icon can fade out after it's gone
    bottom: bool,                      // position: false = top center, true = bottom-right above the taskbar
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn color(rgb: u32, a: f32) -> D2D1_COLOR_F {
    let c = |s: u32| ((rgb >> s) & 0xFF) as f32 / 255.0;
    D2D1_COLOR_F { r: c(16), g: c(8), b: c(0), a }
}

fn rect(l: f32, t: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: l, top: t, right: l + w, bottom: t + h }
}

/// "5h 12j" / "2j 30m" / "12m" (hari / jam / menit)
fn until(ts: Option<i64>) -> String {
    let Some(ts) = ts else { return "-".into() };
    let s = (ts - now_secs()).max(0);
    let (d, h, m) = (s / 86400, s % 86400 / 3600, s % 3600 / 60);
    if d > 0 { format!("{d}h {h}j") } else if h > 0 { format!("{h}j {m}m") } else { format!("{m}m") }
}

fn collapsed_line(s: &State) -> String {
    format!("CPU {:.0}%   RAM {:.0}%   GPU {:.0}%", s.cpu, s.ram, s.gpu)
}

/// Widest possible collapsed line: the pill is sized for this, so changing metrics never resize it.
const COLLAPSED_WIDEST: &str = "CPU 100%   RAM 100%   GPU 100%";

/// Fullscreen video, game or presentation in front (same signal Windows uses to mute toasts).
fn fullscreen() -> bool {
    unsafe { SHQueryUserNotificationState() }
        .is_ok_and(|q| q == QUNS_BUSY || q == QUNS_RUNNING_D3D_FULL_SCREEN || q == QUNS_PRESENTATION_MODE)
}

const BROWSERS: [&str; 9] = [
    "chrome.exe", "msedge.exe", "brave.exe", "firefox.exe", "opera.exe", "vivaldi.exe", "arc.exe", "zen.exe",
    "floorp.exe",
];

/// A browser in front, maximized or fullscreen on the primary monitor: its tab strip sits right
/// under the island, so the island gets out of the way (hides) like it does for fullscreen apps.
fn browser_covers_island() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() {
            return false;
        }
        let mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        if mon != MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) {
            return false;
        }
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mut wr = RECT::default();
        let _ = GetMonitorInfoW(mon, &mut mi);
        let _ = GetWindowRect(fg, &mut wr);
        if !IsZoomed(fg).as_bool() && wr != mi.rcMonitor {
            return false;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        let path = String::from_utf16_lossy(&buf[..len as usize]).to_lowercase();
        ok && BROWSERS.contains(&path.rsplit('\\').next().unwrap_or_default())
    }
}

/// Orange gear while Claude works, green note while media plays.
fn indicator(s: &State) -> Option<(&'static str, u32)> {
    if s.sessions.iter().any(|s| s.busy) {
        Some(("\u{E9F5}", ORANGE))
    } else if s.media.as_ref().is_some_and(|m| m.playing) {
        Some(("\u{EC4F}", 0x4ADE80))
    } else {
        None
    }
}

fn usage_rows(s: &State) -> f32 {
    match &s.usage {
        Some(Ok(u)) => u.len().div_ceil(2).max(1) as f32,
        _ => 1.0,
    }
}

/// Pager pages: Claude quota, plus brightness when a DDC/CI monitor exists.
fn pages(s: &State) -> usize {
    if s.displays.is_empty() { 1 } else { 2 }
}

/// Pager body is as tall as its tallest page, so flipping pages never resizes the island.
fn pager_body_h(s: &State) -> f32 {
    (METER_H * usage_rows(s)).max(BRIGHT_H * s.displays.len() as f32)
}

fn expanded_h(s: &State) -> f32 {
    PAD_TOP
        + if s.media.is_some() { MEDIA_H } else { 0.0 }
        + if s.notifs.is_empty() { 0.0 } else { NOTIF_H }
        + METER_H
        + PAGE_HDR
        + pager_body_h(s)
        + SESSION_H * s.sessions.len().clamp(1, MAX_SESSIONS) as f32
        + PAD_BOTTOM
}

impl App {
    fn text_w(&self, s: &str, fmt: &IDWriteTextFormat) -> f32 {
        let w: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            let Ok(layout) = self.dw.CreateTextLayout(&w, fmt, 2000.0, 50.0) else { return 0.0 };
            let mut m = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut m).map_or(0.0, |_| m.widthIncludingTrailingWhitespace)
        }
    }

    fn layout(&self, s: &State) -> (Mode, [f32; 3]) {
        let flashing = s.flash_at.is_some_and(|t| t.elapsed() < Duration::from_secs(FLASH_SECS));
        if self.hover {
            (Mode::Expanded, [EXPANDED_W, expanded_h(s).min(MAX_H), 30.0])
        } else if flashing && !s.notifs.is_empty() {
            let (_, head, body) = &s.notifs[0];
            let w =self.text_w(head, &self.text).max(self.text_w(body, &self.small)) + 48.0;
            (Mode::Flash, [w.clamp(240.0, MAX_W), 60.0, 26.0])
        } else {
            // Metrics are measured at their widest so they never resize the pill; the indicator icon
            // appearing/disappearing (media playing, Claude working) is allowed to.
            let icon = if indicator(s).is_some() { 26.0 } else { 0.0 };
            let w = self.text_w(COLLAPSED_WIDEST, &self.small) + 34.0 + icon;
            (Mode::Collapsed, [w.clamp(140.0, MAX_W), 34.0, 17.0])
        }
    }

    /// Advance animation one step; returns true while still moving.
    fn step(&mut self) -> bool {
        let st = self.state.clone();
        let s = st.lock().unwrap();
        let t = self.layout(&s).1;
        let mut moving = false;
        for i in 0..3 {
            let d = t[i] - self.cur[i];
            if d.abs() < 0.5 {
                self.cur[i] = t[i];
            } else {
                self.cur[i] += d * 0.3;
                moving = true;
            }
        }
        // Same easing as the width, so text, icon and pill edge move together.
        let d = if indicator(&s).is_some() { 1.0 } else { 0.0 } - self.slot;
        if d.abs() < 0.01 {
            self.slot += d;
        } else {
            self.slot += d * 0.3;
            moving = true;
        }
        moving
    }

    fn kick(&mut self) {
        let flash_at = self.state.lock().unwrap().flash_at;
        if flash_at != self.seen_flash {
            self.seen_flash = flash_at;
            self.notif_idx = 0; // new notification: carousel jumps back to the newest
        }
        if self.sync_screen() {
            return;
        }
        if self.step() {
            unsafe { SetTimer(Some(self.hwnd), ANIM_TIMER, 15, None) };
        }
        self.paint();
    }

    fn scroll_notif(&mut self, delta: i32) {
        let n = self.state.lock().unwrap().notifs.len();
        self.notif_idx = (self.notif_idx as i32 + delta).clamp(0, n.max(1) as i32 - 1) as usize;
        self.paint();
    }

    /// Brightness slider under a client-DIP point: (display index, pct at that x).
    fn slider_at(&self, px: f32, py: f32) -> Option<(usize, u32)> {
        let (r, i) = self.sliders.iter().find(|(r, _)| px >= r.left - 8.0 && px <= r.right + 8.0 && py >= r.top && py <= r.bottom)?;
        Some((*i, ((px - r.left) / (r.right - r.left) * 100.0).round().clamp(0.0, 100.0) as u32))
    }

    fn set_brightness(&mut self, i: usize, pct: u32) {
        if let Some(d) = self.state.lock().unwrap().displays.get_mut(i) {
            d.pct = pct; // optimistic; the worker re-reads the real value on its next scan
        }
        let _ = self.tx.send(Cmd::Brightness(i, pct));
        self.paint();
    }

    /// Drop the notification on screen from the island (and from the Windows notification center).
    fn dismiss_notif(&mut self) {
        let id = {
            let mut s = self.state.lock().unwrap();
            if s.notifs.is_empty() {
                return;
            }
            let i = self.notif_idx.min(s.notifs.len() - 1);
            s.flash_at = None; // don't re-flash the next one after the mouse leaves
            self.notif_idx = i.min(s.notifs.len().saturating_sub(2));
            s.notifs.remove(i).0
        };
        self.seen_flash = None;
        let _ = self.tx.send(Cmd::Dismiss(id));
        self.kick();
    }

    fn paint(&mut self) {
        unsafe {
            let rc = RECT { left: 0, top: 0, right: self.px.cx, bottom: self.px.cy };
            if self.rt.BindDC(self.memdc, &rc).is_err() {
                return;
            }
            self.rt.BeginDraw();
            self.rt.Clear(Some(&color(0, 0.0)));
            self.draw();
            let _ = self.rt.EndDraw(None, None);
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
                ..Default::default()
            };
            let _ = UpdateLayeredWindow(
                self.hwnd,
                None,
                None,
                Some(&self.px),
                Some(self.memdc),
                Some(&POINT::default()),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }

    fn fill(&self, r: D2D_RECT_F, radius: f32, rgb: u32, a: f32) {
        unsafe {
            let Ok(b) = self.rt.CreateSolidColorBrush(&color(rgb, a), None) else { return };
            self.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius }, &b);
        }
    }

    fn label(&self, s: &str, fmt: &IDWriteTextFormat, r: D2D_RECT_F, rgb: u32) {
        self.label_alpha(s, fmt, r, rgb, 1.0);
    }

    fn label_alpha(&self, s: &str, fmt: &IDWriteTextFormat, r: D2D_RECT_F, rgb: u32, a: f32) {
        unsafe {
            let Ok(b) = self.rt.CreateSolidColorBrush(&color(rgb, a), None) else { return };
            let w: Vec<u16> = s.encode_utf16().collect();
            self.rt.DrawText(&w, fmt, &r, &b, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
        }
    }

    fn bar(&self, x: f32, y: f32, w: f32, pct: f32, rgb: u32) {
        self.fill(rect(x, y, w, 4.0), 2.0, DIM, 1.0);
        self.fill(rect(x, y, w * (pct / 100.0).clamp(0.0, 1.0), 4.0), 2.0, rgb, 1.0);
    }

    /// Round icon button; `hit: None` draws it dimmed and unclickable.
    fn button(&mut self, r: D2D_RECT_F, glyph: &str, hit: Option<Hit>) {
        self.fill(r, (r.bottom - r.top) / 2.0, 0x1E1E1E, 1.0);
        self.label(glyph, &self.icons, r, if hit.is_some() { WHITE } else { 0x555555 });
        if let Some(h) = hit {
            self.buttons.push((r, h));
        }
    }

    /// Hide behind fullscreen apps (games, video) and, at the top position, behind a maximized
    /// browser whose tabs would sit under the island. Runs on the 1 s tick and, so the island never
    /// flashes over a window going fullscreen, on every foreground/resize event.
    /// Returns true while hidden.
    fn sync_screen(&mut self) -> bool {
        let hide = fullscreen() || (!self.bottom && browser_covers_island());
        if hide != self.hidden {
            self.hidden = hide;
            self.hover = false;
            let _ = unsafe { ShowWindow(self.hwnd, if hide { SW_HIDE } else { SW_SHOWNOACTIVATE }) };
        }
        hide
    }

    /// Pill's top edge in the canvas: pinned near the top, or growing upward from the bottom corner.
    fn pill_top(&self, h: f32) -> f32 {
        if self.bottom { CANVAS_H - EDGE - h } else { TOP }
    }

    /// Pill's left edge in the canvas: centered at the top, right-aligned in the bottom corner.
    fn pill_left(&self, w: f32) -> f32 {
        if self.bottom { CANVAS_W - EDGE - w } else { (CANVAS_W - w) / 2.0 }
    }

    /// Move the window for the current position (also after display/taskbar changes):
    /// top center of the primary monitor, or its bottom-right corner just above the taskbar.
    fn place(&mut self) {
        unsafe {
            let mon = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
            let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(mon, &mut mi);
            let (screen, work) = (mi.rcMonitor, mi.rcWork); // work area = screen minus taskbar
            let (x, y) = if self.bottom {
                (work.right - self.px.cx, work.bottom - self.px.cy)
            } else {
                (screen.left + (screen.right - screen.left - self.px.cx) / 2, screen.top)
            };
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    fn set_bottom(&mut self, bottom: bool) {
        self.bottom = bottom;
        install::save_position(bottom);
        self.place();
        self.kick();
    }

    fn draw(&mut self) {
        let [w, h, r] = self.cur;
        let x = self.pill_left(w);
        let top = self.pill_top(h);
        self.fill(rect(x, top, w, h), r, 0x000000, 1.0);
        self.buttons.clear();
        self.sliders.clear();

        let st = self.state.clone();
        let s = st.lock().unwrap();
        let (mode, target) = self.layout(&s);
        if self.cur == target {
            self.shown = mode;
        }
        if mode != self.shown {
            return; // only the pill shape while morphing between modes; same-mode resizes keep content
        }

        match mode {
            Mode::Collapsed => {
                // Metrics centered when alone, left-aligned next to the indicator icon otherwise;
                // `slot` slides between the two and fades the icon, both ways.
                if let Some(icon) = indicator(&s) {
                    self.last_icon = Some(icon);
                }
                let line = collapsed_line(&s);
                let centered = x + (w - self.text_w(&line, &self.small)) / 2.0;
                let tx = centered + (x + 17.0 - centered) * self.slot;
                self.label(&line, &self.small, rect(tx, top + 9.0, w, 18.0), WHITE);
                if let (Some((glyph, rgb)), true) = (self.last_icon, self.slot > 0.0) {
                    self.label_alpha(glyph, &self.icons, rect(x + w - 36.0, top + 7.0, 20.0, 20.0), rgb, self.slot);
                }
            }
            Mode::Flash => {
                let (_, head, body) = &s.notifs[0];
                self.label(head, &self.text, rect(x + 24.0, top + 10.0, w - 48.0, 20.0), WHITE);
                self.label(body, &self.small, rect(x + 24.0, top + 32.0, w - 48.0, 18.0), GRAY);
            }
            Mode::Expanded => {
                let (l, cw) = (x + 22.0, w - 44.0);
                let mut y = top + PAD_TOP;

                if let Some(m) = &s.media {
                    self.label(&m.title, &self.text, rect(l, y, cw - 116.0, 20.0), WHITE);
                    self.label(&m.artist, &self.small, rect(l, y + 20.0, cw - 116.0, 18.0), GRAY);
                    let bx = l + cw - 104.0;
                    let toggle = if m.playing { "\u{E769}" } else { "\u{E768}" };
                    let (prev, next) = (m.can_prev, m.can_next);
                    self.button(rect(bx, y + 2.0, 32.0, 32.0), "\u{E892}", prev.then_some(Hit::Media(|| Cmd::Prev)));
                    self.button(rect(bx + 36.0, y + 2.0, 32.0, 32.0), toggle, Some(Hit::Media(|| Cmd::Toggle)));
                    self.button(rect(bx + 72.0, y + 2.0, 32.0, 32.0), "\u{E893}", next.then_some(Hit::Media(|| Cmd::Next)));
                    y += MEDIA_H;
                }

                // Notification carousel: newest first, arrows or mouse wheel to browse, × to dismiss.
                if !s.notifs.is_empty() {
                    let n = s.notifs.len();
                    let i = self.notif_idx.min(n - 1);
                    let (_, head, body) = &s.notifs[i];
                    let tw = if n > 1 { cw - 144.0 } else { cw - 32.0 };
                    self.label(head, &self.text, rect(l, y, tw, 20.0), WHITE);
                    self.label(body, &self.small, rect(l, y + 20.0, tw, 18.0), GRAY);
                    self.button(rect(l + cw - 26.0, y + 6.0, 26.0, 26.0), "\u{E711}", Some(Hit::NotifDismiss));
                    if n > 1 {
                        let bx = l + cw - 130.0;
                        self.button(rect(bx, y + 6.0, 26.0, 26.0), "\u{E76B}", (i > 0).then_some(Hit::NotifPrev));
                        let counter = format!("{}/{n}", i + 1);
                        self.label(&counter, &self.small_center, rect(bx + 26.0, y + 11.0, 46.0, 16.0), GRAY);
                        self.button(rect(bx + 72.0, y + 6.0, 26.0, 26.0), "\u{E76C}", (i + 1 < n).then_some(Hit::NotifNext));
                    }
                    y += NOTIF_H;
                }

                let col = (cw - 24.0) / 3.0;
                for (i, (name, v)) in [("CPU", s.cpu), ("RAM", s.ram), ("GPU", s.gpu)].into_iter().enumerate() {
                    let cx = l + i as f32 * (col + 12.0);
                    self.label(&format!("{name}  {v:.0}%"), &self.small, rect(cx, y, col, 16.0), WHITE);
                    self.bar(cx, y + 20.0, col, v, if v > 85.0 { 0xF87171 } else { 0x60A5FA });
                }
                y += METER_H;

                // Pager: page 1 Claude quota, page 2 monitor brightness; ‹ › and dots in the header.
                let n = pages(&s);
                let page = self.page.min(n - 1);
                let title = ["Kuota Claude", "Kecerahan layar"][page];
                self.label(title, &self.small, rect(l, y + 3.0, cw - 90.0, 16.0), GRAY);
                if n > 1 {
                    let bx = l + cw - 70.0;
                    self.button(rect(bx, y, 22.0, 22.0), "\u{E76B}", (page > 0).then_some(Hit::PagePrev));
                    let dots_x = bx + 22.0 + (26.0 - (n as f32 * 10.0 - 4.0)) / 2.0;
                    for d in 0..n {
                        let rgb = if d == page { WHITE } else { DIM };
                        self.fill(rect(dots_x + d as f32 * 10.0, y + 8.0, 6.0, 6.0), 3.0, rgb, 1.0);
                    }
                    self.button(rect(bx + 48.0, y, 22.0, 22.0), "\u{E76C}", (page + 1 < n).then_some(Hit::PageNext));
                }
                y += PAGE_HDR;
                let body_top = y;

                if page == 0 {
                    match &s.usage {
                        Some(Ok(u)) => {
                            let half = (cw - 12.0) / 2.0;
                            // Two limits per row, as many rows as the plan has limits.
                            for (i, lim) in u.iter().enumerate() {
                                let (cx, cy) = (l + (i % 2) as f32 * (half + 12.0), y + (i / 2) as f32 * METER_H);
                                let line = format!("{}  {:.0}%  · reset {}", lim.label, lim.pct, until(lim.reset));
                                self.label(&line, &self.small, rect(cx, cy, half, 16.0), WHITE);
                                self.bar(cx, cy + 20.0, half, lim.pct as f32, ORANGE);
                            }
                        }
                        Some(Err(e)) => self.label(&format!("Usage Claude: {e}"), &self.small, rect(l, y, cw, 16.0), GRAY),
                        None => self.label("Usage Claude: memuat…", &self.small, rect(l, y, cw, 16.0), GRAY),
                    }
                } else {
                    // Brightness per monitor: click/drag the track, or mouse-wheel over the row.
                    for (i, d) in s.displays.iter().enumerate() {
                        let pct = match self.drag {
                            Some((j, v)) if j == i => v,
                            _ => d.pct,
                        };
                        let (tx, tw) = (l + 76.0, cw - 76.0 - 44.0);
                        self.label("\u{E706}", &self.icons, rect(l, y, 16.0, 16.0), GRAY);
                        self.label(&d.label, &self.small, rect(l + 22.0, y, 54.0, 16.0), WHITE);
                        self.bar(tx, y + 6.0, tw, pct as f32, 0xFACC15);
                        self.fill(rect(tx + tw * pct as f32 / 100.0 - 6.0, y + 2.0, 12.0, 12.0), 6.0, WHITE, 1.0);
                        self.label(&format!("{pct}%"), &self.small, rect(l + cw - 36.0, y, 36.0, 16.0), GRAY);
                        self.sliders.push((rect(tx, y - 4.0, tw, BRIGHT_H), i));
                        y += BRIGHT_H;
                    }
                }
                y = body_top + pager_body_h(&s);

                if s.sessions.is_empty() {
                    self.label("Tidak ada sesi Claude aktif", &self.small, rect(l, y, cw, 16.0), GRAY);
                }
                for sess in s.sessions.iter().take(MAX_SESSIONS) {
                    let (dot, state) = if sess.busy { (ORANGE, "bekerja") } else { (GRAY, "idle") };
                    self.fill(rect(l, y + 5.0, 7.0, 7.0), 3.5, dot, 1.0);
                    self.label(&sess.name, &self.small, rect(l + 14.0, y, cw - 80.0, 16.0), WHITE);
                    self.label(state, &self.small, rect(l + cw - 60.0, y, 60.0, 16.0), GRAY);
                    y += SESSION_H;
                }
            }
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let with = |f: &mut dyn FnMut(&mut App)| {
        APP.with(|a| {
            if let Ok(mut a) = a.try_borrow_mut() {
                if let Some(app) = a.as_mut() {
                    f(app);
                }
            }
        })
    };
    match msg {
        WM_APP => with(&mut |a| a.kick()),
        WM_TIMER => with(&mut |a| {
            if !a.step() {
                unsafe { KillTimer(Some(hwnd), ANIM_TIMER).ok() };
            }
            a.paint();
        }),
        WM_MOUSEMOVE => with(&mut |a| {
            if let Some((i, _)) = a.drag {
                let px = (lp.0 & 0xFFFF) as i16 as f32 / a.scale;
                if let Some((r, _)) = a.sliders.iter().find(|(_, j)| *j == i) {
                    a.drag = Some((i, ((px - r.left) / (r.right - r.left) * 100.0).round().clamp(0.0, 100.0) as u32));
                    a.paint();
                }
            }
            if !a.hover {
                a.hover = true;
                let mut t = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                unsafe { TrackMouseEvent(&mut t).ok() };
                a.kick();
            }
        }),
        WM_MOUSELEAVE => with(&mut |a| {
            if a.drag.is_none() {
                a.hover = false;
                a.kick();
            }
        }),
        WM_MOUSEWHEEL => with(&mut |a| {
            let up = (wp.0 >> 16) as i16 > 0;
            // Wheel coordinates are screen-based.
            let mut pt = POINT { x: (lp.0 & 0xFFFF) as i16 as i32, y: (lp.0 >> 16 & 0xFFFF) as i16 as i32 };
            let _ = unsafe { ScreenToClient(hwnd, &mut pt) };
            let over = a.slider_at(pt.x as f32 / a.scale, pt.y as f32 / a.scale);
            match over {
                Some((i, _)) => {
                    let cur = a.state.lock().unwrap().displays.get(i).map_or(0, |d| d.pct) as i32;
                    let step = if up { BRIGHT_STEP } else { -BRIGHT_STEP };
                    a.set_brightness(i, (cur + step).clamp(0, 100) as u32);
                }
                None => a.scroll_notif(if up { -1 } else { 1 }),
            }
        }),
        WM_LBUTTONDOWN => with(&mut |a| {
            let (px, py) = ((lp.0 & 0xFFFF) as i16 as f32 / a.scale, (lp.0 >> 16 & 0xFFFF) as i16 as f32 / a.scale);
            if let Some(hit) = a.slider_at(px, py) {
                a.drag = Some(hit);
                unsafe { SetCapture(hwnd) };
                a.paint();
            }
        }),
        WM_LBUTTONUP => with(&mut |a| {
            if let Some((i, pct)) = a.drag.take() {
                let _ = unsafe { ReleaseCapture() };
                a.set_brightness(i, pct);
                return;
            }
            let (px, py) = ((lp.0 & 0xFFFF) as i16 as f32 / a.scale, (lp.0 >> 16 & 0xFFFF) as i16 as f32 / a.scale);
            let hit = a.buttons.iter().find(|(r, _)| px >= r.left && px <= r.right && py >= r.top && py <= r.bottom);
            match hit.map(|h| h.1) {
                Some(Hit::Media(cmd)) => {
                    let _ = a.tx.send(cmd());
                }
                Some(Hit::NotifPrev) => a.scroll_notif(-1),
                Some(Hit::NotifNext) => a.scroll_notif(1),
                Some(Hit::NotifDismiss) => a.dismiss_notif(),
                Some(Hit::PagePrev) => {
                    a.page = a.page.saturating_sub(1);
                    a.paint();
                }
                Some(Hit::PageNext) => {
                    a.page = (a.page + 1).min(1);
                    a.paint();
                }
                None => {}
            }
        }),
        WM_RBUTTONUP => unsafe { PostQuitMessage(0) }, // right-click = exit
        WM_DESTROY => unsafe { PostQuitMessage(0) },
        WM_TRAY if matches!((lp.0 & 0xFFFF) as u32, WM_LBUTTONUP | WM_RBUTTONUP) => {
            // The menu runs its own modal loop, so don't hold the App borrow across it.
            let mut bottom = false;
            with(&mut |a| bottom = a.bottom);
            match tray_menu(hwnd, bottom) {
                TRAY_EXIT => unsafe { PostQuitMessage(0) },
                TRAY_TOP => with(&mut |a| a.set_bottom(false)),
                TRAY_BOTTOM => with(&mut |a| a.set_bottom(true)),
                _ => {}
            }
        }
        // Resolution or work area (taskbar size/position) changed.
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => with(&mut |a| a.place()),
        WM_SCREEN_CHANGED => {
            SCREEN_PENDING.store(false, Ordering::Relaxed);
            with(&mut |a| {
                let hidden = a.hidden;
                if a.sync_screen() != hidden {
                    a.kick(); // shown again: repaint now
                }
            });
        }
        // Explorer restarted: its tray forgot us, add the icon again (taskbar may have moved too).
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            tray(hwnd, NIM_ADD);
            with(&mut |a| a.place());
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

fn text_format(dw: &IDWriteFactory, family: PCWSTR, size: f32, center: bool) -> Result<IDWriteTextFormat> {
    unsafe {
        let f = dw.CreateTextFormat(
            family,
            None,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("id-ID"),
        )?;
        f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        let sign = dw.CreateEllipsisTrimmingSign(&f)?;
        let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, ..Default::default() };
        f.SetTrimming(&trim, &sign)?;
        if center {
            f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        }
        Ok(f)
    }
}

const WM_TRAY: u32 = WM_APP + 1;
const TRAY_EXIT: i32 = 1;
const TRAY_TOP: i32 = 2;
const TRAY_BOTTOM: i32 = 3;
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

// Foreground window changed or moved/resized (F11, fullscreen video, maximize): re-check at once.
const WM_SCREEN_CHANGED: u32 = WM_APP + 2;
static ISLAND_HWND: AtomicIsize = AtomicIsize::new(0);
static SCREEN_PENDING: AtomicBool = AtomicBool::new(false); // coalesces bursts (window drags)

unsafe extern "system" fn on_win_event(_: HWINEVENTHOOK, event: u32, hwnd: HWND, obj: i32, _: i32, _: u32, _: u32) {
    if obj != OBJID_WINDOW.0 {
        return;
    }
    // Location changes fire for every window; only the foreground one matters.
    if event == EVENT_OBJECT_LOCATIONCHANGE && hwnd != unsafe { GetForegroundWindow() } {
        return;
    }
    if !SCREEN_PENDING.swap(true, Ordering::Relaxed) {
        let island = HWND(ISLAND_HWND.load(Ordering::Relaxed) as _);
        let _ = unsafe { PostMessageW(Some(island), WM_SCREEN_CHANGED, WPARAM(0), LPARAM(0)) };
    }
}

/// Tray icon (the embedded logo, resource 1) so the island can be quit from the taskbar.
fn tray(hwnd: HWND, action: NOTIFY_ICON_MESSAGE) {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let (cx, cy) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        let icon = LoadImageW(Some(hinst.into()), PCWSTR(1 as _), IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR);
        let mut nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: icon.map(|h| HICON(h.0)).unwrap_or_default(),
            ..Default::default()
        };
        let tip: Vec<u16> = format!("FloatIsland {}", env!("CARGO_PKG_VERSION")).encode_utf16().collect();
        nid.szTip[..tip.len()].copy_from_slice(&tip);
        let _ = Shell_NotifyIconW(action, &nid);
    }
}

/// Shows the tray menu and returns the chosen command (0 = dismissed).
fn tray_menu(hwnd: HWND, bottom: bool) -> i32 {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        let check = |on: bool| if on { MF_STRING | MF_CHECKED } else { MF_STRING };
        let title = HSTRING::from(format!("FloatIsland {}", env!("CARGO_PKG_VERSION")));
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, &title);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, check(!bottom), TRAY_TOP as usize, w!("Posisi: Atas"));
        let _ = AppendMenuW(menu, check(bottom), TRAY_BOTTOM as usize, w!("Posisi: Kanan bawah"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, TRAY_EXIT as usize, w!("Keluar"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd); // lets the menu close when clicking elsewhere
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, None, hwnd, None);
        let _ = DestroyMenu(menu);
        cmd.0
    }
}

fn main() -> Result<()> {
    if install::setup_or_uninstall() {
        return Ok(());
    }
    unsafe {
        // One island per session: a manual launch while the autostarted one runs just exits.
        let _mutex = CreateMutexW(None, true, w!("Local\\FloatIslandSingleton"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }
        // Only the installed copy owns autostart, so dev builds in target\ don't hijack it.
        if install::is_installed_copy() {
            install::register_autostart();
        }

        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let scale = GetDpiForSystem() as f32 / 96.0;
        let px = SIZE { cx: (CANVAS_W * scale) as i32, cy: (CANVAS_H * scale) as i32 };

        let hinst = GetModuleHandleW(None)?;
        let class = w!("FloatIsland");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_HAND)?,
            ..Default::default()
        });
        // ponytail: primary monitor only; App::place() moves it to the top or into the taskbar
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!("FloatIsland"),
            WS_POPUP,
            0,
            0,
            px.cx,
            px.cy,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;

        // 32-bit DIB that Direct2D draws into and UpdateLayeredWindow blits with per-pixel alpha.
        let screen = GetDC(None);
        let memdc = CreateCompatibleDC(Some(screen));
        ReleaseDC(None, screen);
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: px.cx,
                biHeight: -px.cy,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bmp = CreateDIBSection(Some(memdc), &bi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        SelectObject(memdc, bmp.into());

        let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        // Software rasterizer: a pill this small doesn't need the GPU, and skipping it avoids loading
        // the D3D driver (~45 MB private memory on NVIDIA).
        let rt = d2d.CreateDCRenderTarget(&D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            ..Default::default()
        })?;
        rt.SetDpi(96.0 * scale, 96.0 * scale);
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);

        let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let state = Arc::new(Mutex::new(State::default()));
        let (tx, rx) = mpsc::channel();
        let h = hwnd.0 as isize;
        data::spawn(state.clone(), rx, move || {
            let _ = PostMessageW(Some(HWND(h as _)), WM_APP, WPARAM(0), LPARAM(0));
        });

        let mut app = App {
            hwnd,
            state,
            tx,
            rt,
            text: text_format(&dw, w!("Segoe UI Variable Text"), 13.0, false)?,
            small: text_format(&dw, w!("Segoe UI Variable Text"), 11.5, false)?,
            small_center: text_format(&dw, w!("Segoe UI Variable Text"), 11.5, true)?,
            icons: text_format(&dw, w!("Segoe Fluent Icons"), 14.0, true)?,
            dw,
            memdc,
            px,
            scale,
            cur: [0.0; 3],
            hover: false,
            buttons: Vec::new(),
            notif_idx: 0,
            seen_flash: None,
            shown: Mode::Collapsed,
            hidden: false,
            sliders: Vec::new(),
            drag: None,
            page: 0,
            slot: 0.0,
            last_icon: None,
            bottom: install::load_position(),
        };
        app.place();
        app.cur = app.layout(&app.state.lock().unwrap()).1;
        app.paint();
        APP.with(|a| *a.borrow_mut() = Some(app));
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
        tray(hwnd, NIM_ADD);
        ISLAND_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
        for event in [EVENT_SYSTEM_FOREGROUND, EVENT_OBJECT_LOCATIONCHANGE] {
            SetWinEventHook(event, event, None, Some(on_win_event), 0, 0, WINEVENT_OUTOFCONTEXT);
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
        tray(hwnd, NIM_DELETE);
        Ok(())
    }
}
