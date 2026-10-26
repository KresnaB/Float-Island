// Background collectors for Linux. Same State/Cmd as Windows, fed from /proc, sysfs, D-Bus
// (MPRIS media, a notification monitor, logind backlight), ddcutil and Claude Code's own files.
use crate::common::*;
use dbus::blocking::Connection;
use dbus::message::MessageType;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn spawn(state: Arc<Mutex<State>>, rx: mpsc::Receiver<Cmd>) {
    let panels: Arc<Mutex<Vec<Panel>>> = Arc::default();
    {
        let (state, panels) = (state.clone(), panels.clone());
        std::thread::spawn(move || scan_brightness(state, panels)); // ddcutil is slow: own thread
    }
    {
        let state = state.clone();
        std::thread::spawn(move || watch_notifications(state)); // blocking D-Bus monitor loop
    }
    std::thread::spawn(move || {
        let mut cpu = Cpu::default();
        let gpu = Gpu::detect();
        let players = mpris::PlayerFinder::new().ok();
        let session_bus = Connection::new_session().ok();
        let mut was_busy: Vec<(String, bool)> = Vec::new();
        let mut next_usage = Instant::now();

        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Cmd::Brightness(i, pct)) => {
                    if let Some(p) = panels.lock().unwrap().get(i) {
                        p.set(pct);
                    }
                    if let Some(d) = state.lock().unwrap().displays.get_mut(i) {
                        d.pct = pct;
                    }
                    continue; // keep wheel steps snappy
                }
                Ok(Cmd::Dismiss(id)) => {
                    if let (Some(bus), true) = (&session_bus, id != 0) {
                        let p = bus.with_proxy(
                            "org.freedesktop.Notifications",
                            "/org/freedesktop/Notifications",
                            Duration::from_secs(2),
                        );
                        let _: Result<(), _> = p.method_call("org.freedesktop.Notifications", "CloseNotification", (id,));
                    }
                }
                Ok(cmd) => {
                    if let Some(p) = players.as_ref().and_then(|f| f.find_active().ok()) {
                        let _ = match cmd {
                            Cmd::Prev => p.previous(),
                            Cmd::Toggle => p.play_pause(),
                            _ => p.next(),
                        };
                        std::thread::sleep(Duration::from_millis(250)); // let the player update its state
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }

            let sessions = claude_sessions();
            let mut done = Vec::new();
            for (id, s) in &sessions {
                if !s.busy && was_busy.iter().any(|(i, b)| i == id && *b) {
                    done.push((0, "Claude selesai".to_string(), s.name.clone()));
                }
            }
            was_busy = sessions.iter().map(|(i, s)| (i.clone(), s.busy)).collect();

            let usage = (Instant::now() >= next_usage).then(|| {
                next_usage = Instant::now() + Duration::from_secs(300);
                claude_usage()
            });
            let media = players.as_ref().and_then(media);
            let (c, r, g) = (cpu.sample(), ram(), gpu.sample());

            let mut s = state.lock().unwrap();
            (s.cpu, s.ram, s.gpu) = (c, r, g);
            s.media = media;
            s.sessions = sessions.into_iter().map(|(_, s)| s).collect();
            if let Some(u) = usage {
                s.usage = Some(u);
            }
            push_notifs(&mut s, done);
        }
    });
}

fn push_notifs(s: &mut State, fresh: Vec<(u32, String, String)>) {
    if fresh.is_empty() {
        return;
    }
    s.flash_at = Some(Instant::now());
    for n in fresh {
        s.notifs.insert(0, n);
    }
    s.notifs.truncate(MAX_NOTIFS);
}

// ---- CPU / RAM / GPU ----

#[derive(Default)]
struct Cpu {
    idle: u64,
    total: u64,
}

impl Cpu {
    fn sample(&mut self) -> f32 {
        let Ok(stat) = std::fs::read_to_string("/proc/stat") else { return 0.0 };
        // "cpu  user nice system idle iowait irq softirq steal ..."
        let v: Vec<u64> = stat.lines().next().unwrap_or_default().split_whitespace().skip(1).take(8)
            .filter_map(|x| x.parse().ok()).collect();
        if v.len() < 5 {
            return 0.0;
        }
        let (idle, total) = (v[3] + v[4], v.iter().sum::<u64>());
        let (di, dt) = (idle.saturating_sub(self.idle), total.saturating_sub(self.total));
        (self.idle, self.total) = (idle, total);
        if dt == 0 { 0.0 } else { (1.0 - di as f32 / dt as f32) * 100.0 }
    }
}

fn ram() -> f32 {
    let Ok(info) = std::fs::read_to_string("/proc/meminfo") else { return 0.0 };
    let kb = |key: &str| -> f32 {
        info.lines().find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1)?.parse().ok()).unwrap_or(0.0)
    };
    let (total, avail) = (kb("MemTotal:"), kb("MemAvailable:"));
    if total == 0.0 { 0.0 } else { (1.0 - avail / total) * 100.0 }
}

// ponytail: AMD/Intel-xe via sysfs, NVIDIA via NVML; older Intel iGPUs expose no busy counter (shows 0)
enum Gpu {
    Sysfs(PathBuf),
    Nvidia(nvml_wrapper::Nvml),
    None,
}

impl Gpu {
    fn detect() -> Gpu {
        let sysfs = std::fs::read_dir("/sys/class/drm").into_iter().flatten().flatten()
            .map(|e| e.path().join("device/gpu_busy_percent"))
            .find(|p| p.exists());
        match sysfs {
            Some(p) => Gpu::Sysfs(p),
            None => nvml_wrapper::Nvml::init().map(Gpu::Nvidia).unwrap_or(Gpu::None),
        }
    }

    fn sample(&self) -> f32 {
        match self {
            Gpu::Sysfs(p) => std::fs::read_to_string(p).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0.0),
            Gpu::Nvidia(n) => n.device_by_index(0).and_then(|d| d.utilization_rates()).map_or(0.0, |u| u.gpu as f32),
            Gpu::None => 0.0,
        }
    }
}

// ---- Media (MPRIS: browsers, Spotify, mpv, ...) ----

fn media(finder: &mpris::PlayerFinder) -> Option<Media> {
    let p = finder.find_active().ok()?;
    let m = p.get_metadata().ok()?;
    let title = m.title().unwrap_or_default().to_string();
    if title.is_empty() {
        return None;
    }
    Some(Media {
        title,
        artist: m.artists().map(|a| a.join(", ")).unwrap_or_default(),
        playing: p.get_playback_status().ok()? == mpris::PlaybackStatus::Playing,
        can_prev: p.can_go_previous().unwrap_or(false),
        can_next: p.can_go_next().unwrap_or(false),
    })
}

// ---- Notifications: eavesdrop Notify calls on the session bus (any notification daemon) ----

fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace('\n', " ")
}

fn watch_notifications(state: Arc<Mutex<State>>) {
    let Ok(conn) = Connection::new_session() else { return };
    let rules = vec![
        "type='method_call',interface='org.freedesktop.Notifications',member='Notify'",
        "type='method_return'", // the reply carries the id we need to dismiss it later
    ];
    let bus = conn.with_proxy("org.freedesktop.DBus", "/org/freedesktop/DBus", Duration::from_secs(5));
    let monitor: Result<(), dbus::Error> = bus.method_call("org.freedesktop.DBus.Monitoring", "BecomeMonitor", (rules, 0u32));
    if monitor.is_err() {
        return;
    }
    // (sender, serial, head, body) of Notify calls still waiting for their reply
    let mut pending: Vec<(String, u32, String, String)> = Vec::new();
    loop {
        if conn.channel().read_write(Some(Duration::from_secs(60))).is_err() {
            return;
        }
        while let Some(msg) = conn.channel().pop_message() {
            match msg.msg_type() {
                MessageType::MethodCall if msg.member().is_some_and(|m| &*m == "Notify") => {
                    let mut it = msg.iter_init();
                    let app = it.read::<&str>().unwrap_or_default().to_string();
                    let _replaces = it.read::<u32>();
                    let _icon = it.read::<&str>();
                    let summary = strip_markup(it.read::<&str>().unwrap_or_default());
                    let body = strip_markup(it.read::<&str>().unwrap_or_default());
                    let head = if summary.is_empty() { app.clone() } else if app.is_empty() { summary } else { format!("{app} · {summary}") };
                    let sender = msg.sender().map(|s| s.to_string()).unwrap_or_default();
                    pending.push((sender, msg.get_serial().unwrap_or(0), head.clone(), body.clone()));
                    if pending.len() > 32 {
                        pending.remove(0);
                    }
                    push_notifs(&mut state.lock().unwrap(), vec![(0, head, body)]);
                }
                MessageType::MethodReturn => {
                    let (Some(serial), Some(dest)) = (msg.get_reply_serial(), msg.destination()) else { continue };
                    let Some(i) = pending.iter().position(|p| p.1 == serial && p.0.as_str() == &*dest) else { continue };
                    let (_, _, head, body) = pending.remove(i);
                    let id = msg.read1::<u32>().unwrap_or(0);
                    let mut s = state.lock().unwrap();
                    if let Some(n) = s.notifs.iter_mut().find(|n| n.0 == 0 && n.1 == head && n.2 == body) {
                        n.0 = id;
                    }
                }
                _ => {}
            }
        }
    }
}

// ---- Brightness: ddcutil (external monitors) and logind (laptop backlight) ----

enum Panel {
    Ddc(u32),               // ddcutil display number
    Backlight(String, u32), // /sys/class/backlight/<name>, max_brightness
}

impl Panel {
    fn get(&self) -> Option<u32> {
        match self {
            Panel::Ddc(n) => {
                // "VCP 10 C 40 100"
                let out = Command::new("ddcutil").args(["--display", &n.to_string(), "getvcp", "10", "--terse"]).output().ok()?;
                let text = String::from_utf8_lossy(&out.stdout);
                let f: Vec<u32> = text.split_whitespace().skip(3).filter_map(|x| x.parse().ok()).collect();
                (f.len() == 2 && f[1] > 0).then(|| f[0] * 100 / f[1])
            }
            Panel::Backlight(name, max) => {
                let cur: u32 = std::fs::read_to_string(format!("/sys/class/backlight/{name}/brightness")).ok()?.trim().parse().ok()?;
                Some(cur * 100 / (*max).max(1))
            }
        }
    }

    fn set(&self, pct: u32) {
        match self {
            Panel::Ddc(n) => {
                let n = n.to_string();
                // Detached so a slow monitor never blocks the worker.
                std::thread::spawn(move || {
                    let _ = Command::new("ddcutil").args(["--display", &n, "setvcp", "10", &pct.to_string()]).status();
                });
            }
            Panel::Backlight(name, max) => {
                // logind lets the active session set its backlight without root.
                let Ok(bus) = Connection::new_system() else { return };
                let p = bus.with_proxy("org.freedesktop.login1", "/org/freedesktop/login1/session/auto", Duration::from_secs(2));
                let value = pct.min(100) * max / 100;
                let _: Result<(), _> = p.method_call("org.freedesktop.login1.Session", "SetBrightness", ("backlight", name.as_str(), value));
            }
        }
    }
}

fn find_panels() -> Vec<(Panel, String)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/sys/class/backlight").into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let max = std::fs::read_to_string(e.path().join("max_brightness")).ok().and_then(|s| s.trim().parse().ok());
        if let Some(max) = max.filter(|m| *m > 0) {
            out.push((Panel::Backlight(name, max), "Laptop".to_string()));
        }
    }
    // "Display 1\n   I2C bus: /dev/i2c-4\n   Monitor: GSM:LG ULTRAGEAR:..."
    if let Ok(o) = Command::new("ddcutil").args(["detect", "--terse"]).output() {
        let text = String::from_utf8_lossy(&o.stdout);
        let mut current: Option<u32> = None;
        for line in text.lines() {
            if let Some(n) = line.strip_prefix("Display ") {
                current = n.trim().parse().ok();
            } else if let (Some(n), Some(m)) = (current, line.trim().strip_prefix("Monitor:")) {
                let model = m.trim().split(':').nth(1).filter(|s| !s.trim().is_empty()).map(str::trim);
                out.push((Panel::Ddc(n), model.map_or(format!("Layar {n}"), str::to_string)));
                current = None;
            }
        }
    }
    out
}

fn scan_brightness(state: Arc<Mutex<State>>, panels: Arc<Mutex<Vec<Panel>>>) {
    loop {
        // Keep only panels that answered, in the same order as `displays`.
        let (mut usable, mut displays) = (Vec::new(), Vec::new());
        for (p, label) in find_panels() {
            if let Some(pct) = p.get() {
                displays.push(Display { label, pct });
                usable.push(p);
            }
        }
        *panels.lock().unwrap() = usable;
        state.lock().unwrap().displays = displays;
        // ponytail: 60 s re-scan (hot-plug, changes made on the monitor's own buttons)
        std::thread::sleep(Duration::from_secs(60));
    }
}

// ---- Claude Code ----

fn home() -> PathBuf {
    std::env::var_os("HOME").unwrap_or_default().into()
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Live Claude Code sessions from ~/.claude/sessions/<pid>.json (written by Claude Code itself).
fn claude_sessions() -> Vec<(String, Session)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(home().join(".claude/sessions")).into_iter().flatten().flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Some(j) = read_json(&path) else { continue };
        let str = |k: &str| j[k].as_str().unwrap_or_default().to_string();
        let pid = j["pid"].as_u64().unwrap_or(0);
        if pid == 0 || !Path::new(&format!("/proc/{pid}")).exists() {
            continue; // stale file from a closed session
        }
        let mut name = str("name");
        if name.is_empty() {
            name = str("cwd").rsplit('/').next().unwrap_or_default().to_string();
        }
        out.push((str("sessionId"), Session { name, busy: str("status") == "busy" }));
    }
    out.sort_by(|a, b| b.1.busy.cmp(&a.1.busy));
    out
}

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as f64
}

fn token() -> Option<(String, f64)> {
    let j = read_json(&home().join(".claude/.credentials.json"))?;
    let o = &j["claudeAiOauth"];
    Some((o["accessToken"].as_str()?.to_string(), o["expiresAt"].as_f64()?))
}

// ponytail: relies on `claude auth status` refreshing ~/.claude/.credentials.json (observed, not documented)
fn refresh_token() {
    let _ = Command::new("claude").args(["auth", "status"]).output();
}

fn claude_usage() -> Result<Usage, String> {
    for attempt in 0..2 {
        let (tok, exp) = token().ok_or("belum login Claude Code")?;
        if exp > now_ms() + 60_000.0 {
            match fetch_usage(&tok) {
                Ok(u) => return Ok(u),
                Err(401) if attempt == 0 => {}
                Err(0) => return Err("offline".into()),
                Err(code) => return Err(format!("HTTP {code}")),
            }
        }
        refresh_token();
    }
    Err("token kedaluwarsa".into())
}

/// Err(status) on HTTP failure, Err(0) on network/parse failure.
fn fetch_usage(token: &str) -> Result<Usage, i32> {
    let resp = ureq::get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", &format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .call();
    let body = match resp {
        Ok(mut r) => r.body_mut().read_to_string().map_err(|_| 0)?,
        Err(ureq::Error::StatusCode(code)) => return Err(code as i32),
        Err(_) => return Err(0),
    };
    let j: serde_json::Value = serde_json::from_str(&body).map_err(|_| 0)?;
    // `limits` is what the Claude app's usage card renders, per plan, so mirror it as-is.
    let limits = j["limits"].as_array().ok_or(0)?;
    Ok(limits
        .iter()
        .map(|o| Limit {
            label: limit_label(o["kind"].as_str().unwrap_or_default(), o["scope"]["model"]["display_name"].as_str()),
            pct: o["percent"].as_f64().unwrap_or(0.0),
            reset: o["resets_at"].as_str().and_then(parse_iso),
        })
        .collect())
}
