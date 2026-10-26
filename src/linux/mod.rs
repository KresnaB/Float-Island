// Linux front end: the same pill as the Windows build, drawn with cairo + pango in a GTK4 window.
// Placement: wlr-layer-shell on KDE Plasma / Hyprland / Sway; GNOME (no layer-shell) runs the
// window through XWayland as an always-on-top dock window. Tray via StatusNotifierItem (ksni).
mod data;

use crate::common::*;
use gtk::prelude::*;
use gtk::{cairo, gdk, glib, pango};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const APP_ID: &str = "io.github.kresnab.FloatIsland";

// Canvas (logical px) the pill lives in; everything outside the pill is click-through.
const CANVAS_W: f64 = 480.0;
const CANVAS_H: f64 = 350.0;
const TOP: f64 = 6.0;
const EDGE: f64 = 12.0; // bottom-right position: gap to the screen edge / panel
const FLASH_SECS: u64 = 5;

const MAX_W: f64 = 460.0;
const MAX_H: f64 = 340.0;
const EXPANDED_W: f64 = 440.0;
// Expanded section heights (draw() and expanded_h() must agree).
const PAD_TOP: f64 = 16.0;
const PAD_BOTTOM: f64 = 6.0;
const MEDIA_H: f64 = 50.0;
const NOTIF_H: f64 = 50.0;
const METER_H: f64 = 38.0;
const SESSION_H: f64 = 20.0;
const MAX_SESSIONS: usize = 3;
const BRIGHT_H: f64 = 24.0;
const PAGE_HDR: f64 = 28.0;
const BRIGHT_STEP: i32 = 5;

const ORANGE: u32 = 0xD97757;
const GREEN: u32 = 0x4ADE80;
const GRAY: u32 = 0x9A9A9A;
const DIM: u32 = 0x3A3A3A;
const WHITE: u32 = 0xFFFFFF;

/// Widest possible collapsed line: the pill is sized for this, so changing metrics never resize it.
const COLLAPSED_WIDEST: &str = "CPU 100%   RAM 100%   GPU 100%";

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

#[derive(Clone, Copy, PartialEq)]
enum Placement {
    Layer, // wlr-layer-shell (KDE, Hyprland, Sway, ...)
    X11,   // X11 session or GNOME through XWayland
    Plain, // neither: a normal undecorated window
}

#[derive(Clone, Copy)]
enum Icon {
    Prev,
    Play,
    Pause,
    Next,
    Left,
    Right,
    Close,
}

enum TrayMsg {
    Bottom(bool),
    Quit,
}

#[derive(Clone, Copy)]
struct R {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl R {
    fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }
}

fn r(x: f64, y: f64, w: f64, h: f64) -> R {
    R { x, y, w, h }
}

fn rgb(c: u32) -> (f64, f64, f64) {
    (((c >> 16) & 0xFF) as f64 / 255.0, ((c >> 8) & 0xFF) as f64 / 255.0, (c & 0xFF) as f64 / 255.0)
}

fn collapsed_line(s: &State) -> String {
    format!("CPU {:.0}%   RAM {:.0}%   GPU {:.0}%", s.cpu, s.ram, s.gpu)
}

/// Orange dot while Claude works, green note while media plays.
fn indicator(s: &State) -> Option<u32> {
    if s.sessions.iter().any(|s| s.busy) {
        Some(ORANGE)
    } else if s.media.as_ref().is_some_and(|m| m.playing) {
        Some(GREEN)
    } else {
        None
    }
}

fn usage_rows(s: &State) -> f64 {
    match &s.usage {
        Some(Ok(u)) => u.len().div_ceil(2).max(1) as f64,
        _ => 1.0,
    }
}

fn pages(s: &State) -> usize {
    if s.displays.is_empty() { 1 } else { 2 }
}

/// Pager body is as tall as its tallest page, so flipping pages never resizes the island.
fn pager_body_h(s: &State) -> f64 {
    (METER_H * usage_rows(s)).max(BRIGHT_H * s.displays.len() as f64)
}

fn expanded_h(s: &State) -> f64 {
    PAD_TOP
        + if s.media.is_some() { MEDIA_H } else { 0.0 }
        + if s.notifs.is_empty() { 0.0 } else { NOTIF_H }
        + METER_H
        + PAGE_HDR
        + pager_body_h(s)
        + SESSION_H * s.sessions.len().clamp(1, MAX_SESSIONS) as f64
        + PAD_BOTTOM
}

fn font(size: f64) -> pango::FontDescription {
    let mut f = pango::FontDescription::from_string("Sans Semi-Bold");
    f.set_absolute_size(size * pango::SCALE as f64);
    f
}

// ---- settings: ~/.config/floatisland/position ("top" | "bottom-right") ----

fn config_file() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(Into::into)
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("floatisland").join("position")
}

fn load_bottom() -> bool {
    std::fs::read_to_string(config_file()).is_ok_and(|s| s.trim() == "bottom-right")
}

fn save_bottom(bottom: bool) {
    let f = config_file();
    let _ = std::fs::create_dir_all(f.parent().unwrap());
    let _ = std::fs::write(f, if bottom { "bottom-right\n" } else { "top\n" });
}

// ---- tray (StatusNotifierItem: KDE, waybar on Hyprland, AppIndicator extension on GNOME) ----

struct Tray {
    tx: mpsc::Sender<TrayMsg>,
    bottom: bool,
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "floatisland".into()
    }

    fn title(&self) -> String {
        "FloatIsland".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        // 32x32 ARGB32, network byte order, written by assets/make_icon.py
        vec![ksni::Icon { width: 32, height: 32, data: include_bytes!("../../assets/tray-32.argb").to_vec() }]
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        vec![
            StandardItem { label: format!("FloatIsland {}", env!("CARGO_PKG_VERSION")), enabled: false, ..Default::default() }.into(),
            MenuItem::Separator,
            RadioGroup {
                selected: self.bottom as usize,
                select: Box::new(|t: &mut Self, i| {
                    t.bottom = i == 1;
                    let _ = t.tx.send(TrayMsg::Bottom(t.bottom));
                }),
                options: vec![
                    RadioItem { label: "Posisi: Atas".into(), ..Default::default() },
                    RadioItem { label: "Posisi: Kanan bawah".into(), ..Default::default() },
                ],
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Keluar".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(TrayMsg::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

// ---- window placement ----

/// GNOME's compositor has no layer-shell; XWayland lets us place the window and keep it above.
fn wants_x11() -> bool {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_uppercase();
    desktop.contains("GNOME") && std::env::var_os("WAYLAND_DISPLAY").is_some()
}

fn monitor_geometry() -> Option<gdk::Rectangle> {
    let monitors = gdk::Display::default()?.monitors();
    Some(monitors.item(0)?.downcast::<gdk::Monitor>().ok()?.geometry())
}

/// X11: make the window an always-on-top, undecorated dock that skips the taskbar, and move it.
fn x11_place(window: &gtk::ApplicationWindow, x: i32, y: i32, init: bool) {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{AtomEnum, ConfigureWindowAux, ConnectionExt, PropMode};
    use x11rb::wrapper::ConnectionExt as _;
    let Some(xid) = window.surface().and_then(|s| s.downcast::<gdk4_x11::X11Surface>().ok()).map(|s| s.xid() as u32)
    else {
        return;
    };
    let Ok((conn, _)) = x11rb::connect(None) else { return };
    let atom = |name: &str| conn.intern_atom(false, name.as_bytes()).ok().and_then(|c| c.reply().ok()).map_or(0, |r| r.atom);
    if init {
        let _ = conn.change_property32(PropMode::REPLACE, xid, atom("_NET_WM_WINDOW_TYPE"), AtomEnum::ATOM, &[atom("_NET_WM_WINDOW_TYPE_DOCK")]);
        let states = ["_NET_WM_STATE_ABOVE", "_NET_WM_STATE_STICKY", "_NET_WM_STATE_SKIP_TASKBAR", "_NET_WM_STATE_SKIP_PAGER"].map(atom);
        let _ = conn.change_property32(PropMode::REPLACE, xid, atom("_NET_WM_STATE"), AtomEnum::ATOM, &states);
    }
    let _ = conn.configure_window(xid, &ConfigureWindowAux::new().x(x).y(y));
    let _ = conn.flush();
}

// ---- the island ----

struct Ui {
    state: Arc<Mutex<State>>,
    tx: mpsc::Sender<Cmd>,
    window: gtk::ApplicationWindow,
    area: gtk::DrawingArea,
    placement: Placement,
    bottom: bool,
    cur: [f64; 3], // animated (width, height, radius)
    slot: f64,     // animated 0..1: indicator presence (text shift + icon fade)
    last_icon: Option<u32>,
    hover: bool,
    shown: Mode, // mode whose content is on screen; content hides only while switching modes
    animating: bool,
    notif_idx: usize,
    seen_flash: Option<Instant>,
    page: usize,
    pointer: (f64, f64),
    buttons: Vec<(R, Hit)>,
    sliders: Vec<(R, usize)>,
    drag: Option<(usize, u32)>,
}

impl Ui {
    fn text_w(&self, s: &str, size: f64) -> f64 {
        let l = self.area.create_pango_layout(Some(s));
        l.set_font_description(Some(&font(size)));
        l.pixel_size().0 as f64
    }

    fn layout(&self, s: &State) -> (Mode, [f64; 3]) {
        let flashing = s.flash_at.is_some_and(|t| t.elapsed() < Duration::from_secs(FLASH_SECS));
        if self.hover {
            (Mode::Expanded, [EXPANDED_W, expanded_h(s).min(MAX_H), 30.0])
        } else if flashing && !s.notifs.is_empty() {
            let (_, head, body) = &s.notifs[0];
            let w = self.text_w(head, 13.0).max(self.text_w(body, 11.5)) + 48.0;
            (Mode::Flash, [w.clamp(240.0, MAX_W), 60.0, 26.0])
        } else {
            // Metrics are measured at their widest; only the indicator icon may resize the pill.
            let icon = if indicator(s).is_some() { 26.0 } else { 0.0 };
            let w = self.text_w(COLLAPSED_WIDEST, 11.5) + 34.0 + icon;
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
        let d = if indicator(&s).is_some() { 1.0 } else { 0.0 } - self.slot;
        if d.abs() < 0.01 {
            self.slot += d;
        } else {
            self.slot += d * 0.3;
            moving = true;
        }
        moving
    }

    fn pill_left(&self, w: f64) -> f64 {
        if self.bottom { CANVAS_W - EDGE - w } else { (CANVAS_W - w) / 2.0 }
    }

    fn pill_top(&self, h: f64) -> f64 {
        if self.bottom { CANVAS_H - EDGE - h } else { TOP }
    }

    fn place(&self) {
        match self.placement {
            Placement::Layer => {
                let w = &self.window;
                w.set_anchor(Edge::Top, !self.bottom);
                w.set_anchor(Edge::Bottom, self.bottom);
                w.set_anchor(Edge::Right, self.bottom);
                w.set_anchor(Edge::Left, false);
            }
            Placement::X11 => {
                let Some(g) = monitor_geometry() else { return };
                let (cw, ch) = (CANVAS_W as i32, CANVAS_H as i32);
                let (x, y) = if self.bottom {
                    (g.x() + g.width() - cw, g.y() + g.height() - ch)
                } else {
                    (g.x() + (g.width() - cw) / 2, g.y())
                };
                x11_place(&self.window, x, y, false);
            }
            Placement::Plain => {}
        }
    }

    fn scroll_notif(&mut self, delta: i32) {
        let n = self.state.lock().unwrap().notifs.len();
        self.notif_idx = (self.notif_idx as i32 + delta).clamp(0, n.max(1) as i32 - 1) as usize;
        self.area.queue_draw();
    }

    fn slider_at(&self, px: f64, py: f64) -> Option<(usize, u32)> {
        let (r, i) = self.sliders.iter().find(|(r, _)| r.contains(px.clamp(r.x, r.x + r.w), py) && px >= r.x - 8.0 && px <= r.x + r.w + 8.0)?;
        Some((*i, ((px - r.x) / r.w * 100.0).round().clamp(0.0, 100.0) as u32))
    }

    fn set_brightness(&mut self, i: usize, pct: u32) {
        if let Some(d) = self.state.lock().unwrap().displays.get_mut(i) {
            d.pct = pct; // optimistic; the scanner re-reads the real value later
        }
        let _ = self.tx.send(Cmd::Brightness(i, pct));
        self.area.queue_draw();
    }

    fn dismiss_notif(&mut self) {
        let id = {
            let mut s = self.state.lock().unwrap();
            if s.notifs.is_empty() {
                return;
            }
            let i = self.notif_idx.min(s.notifs.len() - 1);
            s.flash_at = None;
            self.notif_idx = i.min(s.notifs.len().saturating_sub(2));
            s.notifs.remove(i).0
        };
        self.seen_flash = None;
        let _ = self.tx.send(Cmd::Dismiss(id));
    }

    // -- drawing helpers --

    fn fill(cr: &cairo::Context, b: R, radius: f64, color: u32, a: f64) {
        let rad = radius.min(b.h / 2.0).min(b.w / 2.0);
        let (x, y, w, h) = (b.x, b.y, b.w, b.h);
        cr.new_sub_path();
        cr.arc(x + w - rad, y + rad, rad, -std::f64::consts::FRAC_PI_2, 0.0);
        cr.arc(x + w - rad, y + h - rad, rad, 0.0, std::f64::consts::FRAC_PI_2);
        cr.arc(x + rad, y + h - rad, rad, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
        cr.arc(x + rad, y + rad, rad, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
        cr.close_path();
        let (cr_, cg, cb) = rgb(color);
        cr.set_source_rgba(cr_, cg, cb, a);
        let _ = cr.fill();
    }

    fn label(cr: &cairo::Context, s: &str, size: f64, b: R, color: u32, a: f64, center: bool) {
        let l = pangocairo::functions::create_layout(cr);
        l.set_font_description(Some(&font(size)));
        l.set_text(s);
        l.set_width((b.w * pango::SCALE as f64) as i32);
        l.set_ellipsize(pango::EllipsizeMode::End);
        if center {
            l.set_alignment(pango::Alignment::Center);
        }
        let dy = if center { (b.h - l.pixel_size().1 as f64) / 2.0 } else { 0.0 };
        let (cr_, cg, cb) = rgb(color);
        cr.set_source_rgba(cr_, cg, cb, a);
        cr.move_to(b.x, b.y + dy);
        pangocairo::functions::show_layout(cr, &l);
    }

    fn bar(cr: &cairo::Context, x: f64, y: f64, w: f64, pct: f64, color: u32) {
        Self::fill(cr, r(x, y, w, 4.0), 2.0, DIM, 1.0);
        Self::fill(cr, r(x, y, w * (pct / 100.0).clamp(0.0, 1.0), 4.0), 2.0, color, 1.0);
    }

    /// Small vector icons, so nothing depends on which symbol fonts are installed.
    fn icon(cr: &cairo::Context, kind: Icon, cx: f64, cy: f64, color: u32) {
        let (cr_, cg, cb) = rgb(color);
        cr.set_source_rgba(cr_, cg, cb, 1.0);
        cr.set_line_width(2.0);
        cr.set_line_cap(cairo::LineCap::Round);
        let tri = |x0: f64, dir: f64| {
            cr.move_to(x0, cy - 5.0);
            cr.line_to(x0 + 8.0 * dir, cy);
            cr.line_to(x0, cy + 5.0);
            cr.close_path();
            let _ = cr.fill();
        };
        match kind {
            Icon::Play => tri(cx - 3.0, 1.0),
            Icon::Pause => {
                cr.rectangle(cx - 4.5, cy - 5.0, 3.0, 10.0);
                cr.rectangle(cx + 1.5, cy - 5.0, 3.0, 10.0);
                let _ = cr.fill();
            }
            Icon::Prev | Icon::Next => {
                let dir = if matches!(kind, Icon::Next) { 1.0 } else { -1.0 };
                tri(cx - 4.0 * dir, dir);
                cr.rectangle(cx + 4.0 * dir - 1.0, cy - 5.0, 2.0, 10.0);
                let _ = cr.fill();
            }
            Icon::Left | Icon::Right => {
                let dir = if matches!(kind, Icon::Right) { 1.0 } else { -1.0 };
                cr.move_to(cx - 2.0 * dir, cy - 4.5);
                cr.line_to(cx + 2.0 * dir, cy);
                cr.line_to(cx - 2.0 * dir, cy + 4.5);
                let _ = cr.stroke();
            }
            Icon::Close => {
                cr.move_to(cx - 4.0, cy - 4.0);
                cr.line_to(cx + 4.0, cy + 4.0);
                cr.move_to(cx + 4.0, cy - 4.0);
                cr.line_to(cx - 4.0, cy + 4.0);
                let _ = cr.stroke();
            }
        }
    }

    /// Round icon button; `hit: None` draws it dimmed and unclickable.
    fn button(&mut self, cr: &cairo::Context, b: R, kind: Icon, hit: Option<Hit>) {
        Self::fill(cr, b, b.h / 2.0, 0x1E1E1E, 1.0);
        Self::icon(cr, kind, b.x + b.w / 2.0, b.y + b.h / 2.0, if hit.is_some() { WHITE } else { 0x555555 });
        if let Some(h) = hit {
            self.buttons.push((b, h));
        }
    }

    fn draw(&mut self, cr: &cairo::Context) {
        cr.set_operator(cairo::Operator::Source);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
        let _ = cr.paint();
        cr.set_operator(cairo::Operator::Over);

        let [w, h, rad] = self.cur;
        let (x, top) = (self.pill_left(w), self.pill_top(h));
        Self::fill(cr, r(x, top, w, h), rad, 0x000000, 1.0);
        self.buttons.clear();
        self.sliders.clear();

        // Only the pill takes input; the rest of the canvas is click-through.
        if let Some(surface) = self.window.surface() {
            let rect = cairo::RectangleInt::new(x as i32, top as i32, w.ceil() as i32, h.ceil() as i32);
            surface.set_input_region(&cairo::Region::create_rectangle(&rect));
        }

        let st = self.state.clone();
        let s = st.lock().unwrap();
        let (mode, target) = self.layout(&s);
        if self.cur == target {
            self.shown = mode;
        }
        if mode != self.shown {
            return; // only the pill shape while morphing between modes
        }

        match mode {
            Mode::Collapsed => {
                // Centered alone, left-aligned next to the indicator; `slot` slides between them.
                if let Some(c) = indicator(&s) {
                    self.last_icon = Some(c);
                }
                let line = collapsed_line(&s);
                let centered = x + (w - self.text_w(&line, 11.5)) / 2.0;
                let tx = centered + (x + 17.0 - centered) * self.slot;
                Self::label(cr, &line, 11.5, r(tx, top + 9.0, w, 18.0), WHITE, 1.0, false);
                if let (Some(c), true) = (self.last_icon, self.slot > 0.0) {
                    let (cr_, cg, cb) = rgb(c);
                    cr.set_source_rgba(cr_, cg, cb, self.slot);
                    cr.arc(x + w - 26.0, top + h / 2.0, 5.0, 0.0, std::f64::consts::TAU);
                    let _ = cr.fill();
                }
            }
            Mode::Flash => {
                let (_, head, body) = &s.notifs[0];
                Self::label(cr, head, 13.0, r(x + 24.0, top + 10.0, w - 48.0, 20.0), WHITE, 1.0, false);
                Self::label(cr, body, 11.5, r(x + 24.0, top + 32.0, w - 48.0, 18.0), GRAY, 1.0, false);
            }
            Mode::Expanded => {
                let (l, cw) = (x + 22.0, w - 44.0);
                let mut y = top + PAD_TOP;

                if let Some(m) = &s.media {
                    Self::label(cr, &m.title, 13.0, r(l, y, cw - 116.0, 20.0), WHITE, 1.0, false);
                    Self::label(cr, &m.artist, 11.5, r(l, y + 20.0, cw - 116.0, 18.0), GRAY, 1.0, false);
                    let bx = l + cw - 104.0;
                    let toggle = if m.playing { Icon::Pause } else { Icon::Play };
                    let (prev, next) = (m.can_prev, m.can_next);
                    self.button(cr, r(bx, y + 2.0, 32.0, 32.0), Icon::Prev, prev.then_some(Hit::Media(|| Cmd::Prev)));
                    self.button(cr, r(bx + 36.0, y + 2.0, 32.0, 32.0), toggle, Some(Hit::Media(|| Cmd::Toggle)));
                    self.button(cr, r(bx + 72.0, y + 2.0, 32.0, 32.0), Icon::Next, next.then_some(Hit::Media(|| Cmd::Next)));
                    y += MEDIA_H;
                }

                if !s.notifs.is_empty() {
                    let n = s.notifs.len();
                    let i = self.notif_idx.min(n - 1);
                    let (_, head, body) = &s.notifs[i];
                    let tw = if n > 1 { cw - 144.0 } else { cw - 32.0 };
                    Self::label(cr, head, 13.0, r(l, y, tw, 20.0), WHITE, 1.0, false);
                    Self::label(cr, body, 11.5, r(l, y + 20.0, tw, 18.0), GRAY, 1.0, false);
                    self.button(cr, r(l + cw - 26.0, y + 6.0, 26.0, 26.0), Icon::Close, Some(Hit::NotifDismiss));
                    if n > 1 {
                        let bx = l + cw - 130.0;
                        self.button(cr, r(bx, y + 6.0, 26.0, 26.0), Icon::Left, (i > 0).then_some(Hit::NotifPrev));
                        Self::label(cr, &format!("{}/{n}", i + 1), 11.5, r(bx + 26.0, y + 6.0, 46.0, 26.0), GRAY, 1.0, true);
                        self.button(cr, r(bx + 72.0, y + 6.0, 26.0, 26.0), Icon::Right, (i + 1 < n).then_some(Hit::NotifNext));
                    }
                    y += NOTIF_H;
                }

                let col = (cw - 24.0) / 3.0;
                for (i, (name, v)) in [("CPU", s.cpu), ("RAM", s.ram), ("GPU", s.gpu)].into_iter().enumerate() {
                    let cx = l + i as f64 * (col + 12.0);
                    Self::label(cr, &format!("{name}  {v:.0}%"), 11.5, r(cx, y, col, 16.0), WHITE, 1.0, false);
                    Self::bar(cr, cx, y + 20.0, col, v as f64, if v > 85.0 { 0xF87171 } else { 0x60A5FA });
                }
                y += METER_H;

                // Pager: page 1 Claude quota, page 2 monitor brightness.
                let n = pages(&s);
                let page = self.page.min(n - 1);
                let title = ["Kuota Claude", "Kecerahan layar"][page];
                Self::label(cr, title, 11.5, r(l, y + 3.0, cw - 90.0, 16.0), GRAY, 1.0, false);
                if n > 1 {
                    let bx = l + cw - 70.0;
                    self.button(cr, r(bx, y, 22.0, 22.0), Icon::Left, (page > 0).then_some(Hit::PagePrev));
                    let dots_x = bx + 22.0 + (26.0 - (n as f64 * 10.0 - 4.0)) / 2.0;
                    for d in 0..n {
                        Self::fill(cr, r(dots_x + d as f64 * 10.0, y + 8.0, 6.0, 6.0), 3.0, if d == page { WHITE } else { DIM }, 1.0);
                    }
                    self.button(cr, r(bx + 48.0, y, 22.0, 22.0), Icon::Right, (page + 1 < n).then_some(Hit::PageNext));
                }
                y += PAGE_HDR;
                let body_top = y;

                if page == 0 {
                    match &s.usage {
                        Some(Ok(u)) => {
                            let half = (cw - 12.0) / 2.0;
                            for (i, lim) in u.iter().enumerate() {
                                let (cx, cy) = (l + (i % 2) as f64 * (half + 12.0), y + (i / 2) as f64 * METER_H);
                                let line = format!("{}  {:.0}%  · reset {}", lim.label, lim.pct, until(lim.reset));
                                Self::label(cr, &line, 11.5, r(cx, cy, half, 16.0), WHITE, 1.0, false);
                                Self::bar(cr, cx, cy + 20.0, half, lim.pct, ORANGE);
                            }
                        }
                        Some(Err(e)) => Self::label(cr, &format!("Usage Claude: {e}"), 11.5, r(l, y, cw, 16.0), GRAY, 1.0, false),
                        None => Self::label(cr, "Usage Claude: memuat…", 11.5, r(l, y, cw, 16.0), GRAY, 1.0, false),
                    }
                } else {
                    for (i, d) in s.displays.iter().enumerate() {
                        let pct = match self.drag {
                            Some((j, v)) if j == i => v,
                            _ => d.pct,
                        };
                        let (tx, tw) = (l + 76.0, cw - 76.0 - 44.0);
                        Self::label(cr, &d.label, 11.5, r(l, y, 72.0, 16.0), WHITE, 1.0, false);
                        Self::bar(cr, tx, y + 6.0, tw, pct as f64, 0xFACC15);
                        Self::fill(cr, r(tx + tw * pct as f64 / 100.0 - 6.0, y + 2.0, 12.0, 12.0), 6.0, WHITE, 1.0);
                        Self::label(cr, &format!("{pct}%"), 11.5, r(l + cw - 36.0, y, 36.0, 16.0), GRAY, 1.0, false);
                        self.sliders.push((r(tx, y - 4.0, tw, BRIGHT_H), i));
                        y += BRIGHT_H;
                    }
                }
                y = body_top + pager_body_h(&s);

                if s.sessions.is_empty() {
                    Self::label(cr, "Tidak ada sesi Claude aktif", 11.5, r(l, y, cw, 16.0), GRAY, 1.0, false);
                }
                for sess in s.sessions.iter().take(MAX_SESSIONS) {
                    let (dot, state) = if sess.busy { (ORANGE, "bekerja") } else { (GRAY, "idle") };
                    Self::fill(cr, r(l, y + 5.0, 7.0, 7.0), 3.5, dot, 1.0);
                    Self::label(cr, &sess.name, 11.5, r(l + 14.0, y, cw - 80.0, 16.0), WHITE, 1.0, false);
                    Self::label(cr, state, 11.5, r(l + cw - 60.0, y, 60.0, 16.0), GRAY, 1.0, false);
                    y += SESSION_H;
                }
            }
        }
    }
}

/// Re-evaluate the target size and animate toward it on the frame clock until it settles.
fn kick(ui: &Rc<RefCell<Ui>>) {
    let mut u = ui.borrow_mut();
    let flash_at = u.state.lock().unwrap().flash_at;
    if flash_at != u.seen_flash {
        u.seen_flash = flash_at;
        u.notif_idx = 0; // new notification: carousel jumps back to the newest
    }
    u.area.queue_draw();
    if u.animating || !u.step() {
        return;
    }
    u.animating = true;
    let ui2 = ui.clone();
    u.area.add_tick_callback(move |area, _| {
        let mut u = ui2.borrow_mut();
        let moving = u.step();
        area.queue_draw();
        if moving {
            glib::ControlFlow::Continue
        } else {
            u.animating = false;
            glib::ControlFlow::Break
        }
    });
}

fn build(app: &gtk::Application) {
    if !app.windows().is_empty() {
        return; // second launch: the single instance is already running
    }
    let window = gtk::ApplicationWindow::new(app);
    window.set_title(Some("FloatIsland"));
    window.set_decorated(false);
    window.set_resizable(false);
    window.set_default_size(CANVAS_W as i32, CANVAS_H as i32);
    let area = gtk::DrawingArea::new();
    area.set_content_width(CANVAS_W as i32);
    area.set_content_height(CANVAS_H as i32);
    window.set_child(Some(&area));

    let css = gtk::CssProvider::new();
    css.load_from_data("window, window.background { background: transparent; box-shadow: none; }");
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }

    let placement = if gtk4_layer_shell::is_supported() {
        window.init_layer_shell();
        window.set_layer(Layer::Top);
        window.set_namespace(Some("floatisland"));
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_exclusive_zone(0);
        Placement::Layer
    } else if gdk::Display::default().is_some_and(|d| d.downcast_ref::<gdk4_x11::X11Display>().is_some()) {
        Placement::X11
    } else {
        Placement::Plain
    };

    let state = Arc::new(Mutex::new(State::default()));
    let (tx, rx) = mpsc::channel();
    data::spawn(state.clone(), rx);

    let bottom = load_bottom();
    let ui = Rc::new(RefCell::new(Ui {
        state,
        tx,
        window: window.clone(),
        area: area.clone(),
        placement,
        bottom,
        cur: [236.0, 34.0, 17.0],
        slot: 0.0,
        last_icon: None,
        hover: false,
        shown: Mode::Collapsed,
        animating: false,
        notif_idx: 0,
        seen_flash: None,
        page: 0,
        pointer: (0.0, 0.0),
        buttons: Vec::new(),
        sliders: Vec::new(),
        drag: None,
    }));

    {
        let ui = ui.clone();
        area.set_draw_func(move |_, cr, _, _| ui.borrow_mut().draw(cr));
    }

    // Hover / drag tracking.
    let motion = gtk::EventControllerMotion::new();
    {
        let ui = ui.clone();
        motion.connect_motion(move |_, px, py| {
            let changed = {
                let mut u = ui.borrow_mut();
                u.pointer = (px, py);
                if let Some((i, _)) = u.drag {
                    if let Some((sr, _)) = u.sliders.iter().find(|(_, j)| *j == i).copied() {
                        u.drag = Some((i, ((px - sr.x) / sr.w * 100.0).round().clamp(0.0, 100.0) as u32));
                        u.area.queue_draw();
                    }
                }
                let [w, h, _] = u.cur;
                let inside = r(u.pill_left(w), u.pill_top(h), w, h).contains(px, py);
                let changed = inside != u.hover && u.drag.is_none();
                if changed {
                    u.hover = inside;
                }
                changed
            };
            if changed {
                kick(&ui);
            }
        });
    }
    {
        let ui = ui.clone();
        motion.connect_leave(move |_| {
            let leave = {
                let mut u = ui.borrow_mut();
                let leave = u.drag.is_none() && u.hover;
                if leave {
                    u.hover = false;
                }
                leave
            };
            if leave {
                kick(&ui);
            }
        });
    }
    area.add_controller(motion);

    // Clicks: buttons, brightness drag, right-click to quit.
    let click = gtk::GestureClick::new();
    click.set_button(0);
    {
        let ui = ui.clone();
        click.connect_pressed(move |g, _, px, py| {
            if g.current_button() == 3 {
                if let Some(app) = ui.borrow().window.application() {
                    glib::idle_add_local_once(move || app.quit());
                }
                return;
            }
            let mut u = ui.borrow_mut();
            if let Some(hit) = u.slider_at(px, py) {
                u.drag = Some(hit);
                u.area.queue_draw();
            }
        });
    }
    {
        let ui = ui.clone();
        click.connect_released(move |g, _, px, py| {
            if g.current_button() != 1 {
                return;
            }
            let mut u = ui.borrow_mut();
            if let Some((i, pct)) = u.drag.take() {
                u.set_brightness(i, pct);
                return;
            }
            let hit = u.buttons.iter().find(|(b, _)| b.contains(px, py)).map(|h| h.1);
            match hit {
                Some(Hit::Media(cmd)) => {
                    let _ = u.tx.send(cmd());
                }
                Some(Hit::NotifPrev) => u.scroll_notif(-1),
                Some(Hit::NotifNext) => u.scroll_notif(1),
                Some(Hit::NotifDismiss) => {
                    u.dismiss_notif();
                    drop(u);
                    kick(&ui);
                }
                Some(Hit::PagePrev) => {
                    u.page = u.page.saturating_sub(1);
                    u.area.queue_draw();
                }
                Some(Hit::PageNext) => {
                    u.page = (u.page + 1).min(1);
                    u.area.queue_draw();
                }
                None => {}
            }
        });
    }
    area.add_controller(click);

    // Wheel: brightness over a slider, otherwise the notification carousel.
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    {
        let ui = ui.clone();
        scroll.connect_scroll(move |_, _, dy| {
            let mut u = ui.borrow_mut();
            let (px, py) = u.pointer;
            match u.slider_at(px, py) {
                Some((i, _)) => {
                    let cur = u.state.lock().unwrap().displays.get(i).map_or(0, |d| d.pct) as i32;
                    let step = if dy < 0.0 { BRIGHT_STEP } else { -BRIGHT_STEP };
                    u.set_brightness(i, (cur + step).clamp(0, 100) as u32);
                }
                None => u.scroll_notif(if dy < 0.0 { -1 } else { 1 }),
            }
            glib::Propagation::Stop
        });
    }
    area.add_controller(scroll);

    // X11 needs the dock hints set before the window is mapped, and a move once it is.
    if placement == Placement::X11 {
        window.connect_realize(|w| x11_place(w, 0, 0, true));
        let ui = ui.clone();
        window.connect_map(move |_| ui.borrow().place());
    } else {
        ui.borrow().place();
    }

    // Tray menu (runs on its own D-Bus thread) -> UI via a channel polled below.
    let (tray_tx, tray_rx) = mpsc::channel();
    let _tray = {
        use ksni::blocking::TrayMethods;
        Tray { tx: tray_tx, bottom }.spawn().ok()
    };

    {
        let ui = ui.clone();
        let app = app.clone();
        glib::timeout_add_local(Duration::from_millis(500), move || {
            let _keep_tray_alive = &_tray;
            while let Ok(msg) = tray_rx.try_recv() {
                match msg {
                    TrayMsg::Quit => {
                        app.quit();
                        return glib::ControlFlow::Break;
                    }
                    TrayMsg::Bottom(b) => {
                        save_bottom(b);
                        let mut u = ui.borrow_mut();
                        u.bottom = b;
                        u.place();
                    }
                }
            }
            kick(&ui);
            glib::ControlFlow::Continue
        });
    }

    window.present();
}

pub fn main() {
    if wants_x11() {
        // SAFETY: single-threaded at this point, before GTK or any worker thread starts.
        unsafe { std::env::set_var("GDK_BACKEND", "x11") };
    }
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build);
    app.run_with_args::<&str>(&[]);
}
