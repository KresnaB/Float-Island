// Background collector: polls every source once per second on one worker thread,
// writes into shared State and wakes the UI thread.
use std::os::windows::process::CommandExt;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::Data::Json::JsonObject;
use windows::Foundation::Uri;
use windows::Media::Control::*;
use windows::UI::Notifications::Management::UserNotificationListener;
use windows::UI::Notifications::{KnownNotificationBindings, NotificationKinds};
use windows::Web::Http::{HttpClient, HttpMethod, HttpRequestMessage, HttpStatusCode};
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Performance::*;
use windows::Win32::System::SystemInformation::*;
use windows::Win32::System::Threading::*;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::core::*;

pub use crate::common::*;

pub fn spawn(state: Arc<Mutex<State>>, rx: mpsc::Receiver<Cmd>, wake: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED).ok() };
        let mut cpu = Cpu::default();
        let gpu = Gpu::new();
        let media_mgr = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
            .and_then(|a| a.join())
            .ok();
        let listener = UserNotificationListener::Current().ok();
        if let Some(l) = &listener {
            let _ = l.RequestAccessAsync().and_then(|a| a.join());
        }
        // Start from the newest existing toast so old ones don't flash on launch.
        let mut last_notif = listener.as_ref().map_or(0, |l| toasts(l).iter().map(|t| t.0).max().unwrap_or(0));
        let mut was_busy: Vec<(String, bool)> = Vec::new();
        let mut next_usage = Instant::now();
        let mut panels: Vec<Panel> = Vec::new();
        let mut next_panels = Instant::now();

        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Cmd::Brightness(i, pct)) => {
                    if let Some(p) = panels.get(i) {
                        p.set(pct);
                        if let Some(d) = state.lock().unwrap().displays.get_mut(i) {
                            d.pct = pct;
                        }
                    }
                    continue; // DDC/CI is slow; skip the full poll so wheel steps stay snappy
                }
                Ok(Cmd::Dismiss(id)) => {
                    if let (Some(l), true) = (&listener, id != 0) {
                        let _ = l.RemoveNotification(id);
                    }
                }
                Ok(cmd) => {
                    if let Some(s) = media_mgr.as_ref().and_then(|m| m.GetCurrentSession().ok()) {
                        let op = match cmd {
                            Cmd::Prev => s.TrySkipPreviousAsync(),
                            Cmd::Toggle => s.TryTogglePlayPauseAsync(),
                            _ => s.TrySkipNextAsync(),
                        };
                        let _ = op.and_then(|o| o.join());
                        // The app applies the command asynchronously; give it a moment so the
                        // poll below already shows the new play state.
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }

            let mut fresh = Vec::new();
            let sessions: Vec<(String, Session)> = claude_sessions();
            for (id, s) in &sessions {
                if !s.busy && was_busy.iter().any(|(i, b)| i == id && *b) {
                    fresh.push((0, "Claude selesai".to_string(), s.name.clone()));
                }
            }
            was_busy = sessions.iter().map(|(i, s)| (i.clone(), s.busy)).collect();

            if let Some(l) = &listener {
                for (id, app, title, body) in toasts(l) {
                    if id > last_notif {
                        last_notif = id;
                        let head = if title.is_empty() { app } else { format!("{app} · {title}") };
                        fresh.push((id, head, body));
                    }
                }
            }

            let usage = (Instant::now() >= next_usage).then(|| {
                next_usage = Instant::now() + Duration::from_secs(300);
                claude_usage()
            });

            // Re-scan monitors (hot-plug, brightness changed from the monitor's own buttons).
            // ponytail: 10 s poll; a WM_DISPLAYCHANGE hook would catch hot-plug instantly
            let displays = (Instant::now() >= next_panels).then(|| {
                next_panels = Instant::now() + Duration::from_secs(10);
                panels = scan_panels();
                panels.iter().map(|p| Display { label: p.label.clone(), pct: p.get() }).collect::<Vec<_>>()
            });

            let media = media_mgr.as_ref().and_then(media);
            let (c, r, g) = (cpu.sample(), ram(), gpu.as_ref().map_or(0.0, Gpu::sample));
            {
                let mut s = state.lock().unwrap();
                s.cpu = c;
                s.ram = r;
                s.gpu = g;
                s.media = media;
                s.sessions = sessions.into_iter().map(|(_, s)| s).collect();
                if let Some(u) = usage {
                    s.usage = Some(u);
                }
                if let Some(d) = displays {
                    s.displays = d;
                }
                if !fresh.is_empty() {
                    s.flash_at = Some(Instant::now());
                    for n in fresh {
                        s.notifs.insert(0, n);
                    }
                    s.notifs.truncate(MAX_NOTIFS);
                }
            }
            wake();
        }
    });
}

// ---- CPU / RAM / GPU ----

#[derive(Default)]
struct Cpu {
    idle: u64,
    total: u64,
}

fn ft(f: FILETIME) -> u64 {
    (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64
}

impl Cpu {
    fn sample(&mut self) -> f32 {
        let (mut i, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        if unsafe { GetSystemTimes(Some(&mut i), Some(&mut k), Some(&mut u)) }.is_err() {
            return 0.0;
        }
        let (idle, total) = (ft(i), ft(k) + ft(u)); // kernel time includes idle
        let (di, dt) = (idle - self.idle, total - self.total);
        (self.idle, self.total) = (idle, total);
        if dt == 0 { 0.0 } else { (1.0 - di as f32 / dt as f32) * 100.0 }
    }
}

fn ram() -> f32 {
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    unsafe { GlobalMemoryStatusEx(&mut m) }.map_or(0.0, |_| m.dwMemoryLoad as f32)
}

struct Gpu {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
}

impl Gpu {
    fn new() -> Option<Gpu> {
        let (mut query, mut counter) = (PDH_HQUERY::default(), PDH_HCOUNTER::default());
        unsafe {
            if PdhOpenQueryW(None, 0, &mut query) != 0 {
                return None;
            }
            // ponytail: sums 3D engine load only (what Task Manager shows for games/video); add Compute/Copy engines if needed
            let path = w!("\\GPU Engine(*engtype_3D)\\Utilization Percentage");
            if PdhAddEnglishCounterW(query, path, 0, &mut counter) != 0 {
                return None;
            }
            PdhCollectQueryData(query);
        }
        Some(Gpu { query, counter })
    }

    fn sample(&self) -> f32 {
        unsafe {
            PdhCollectQueryData(self.query);
            let (mut size, mut count) = (0u32, 0u32);
            PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, None);
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items)) != 0 {
                return 0.0;
            }
            let items = std::slice::from_raw_parts(items, count as usize);
            items.iter().map(|it| it.FmtValue.Anonymous.doubleValue).sum::<f64>().min(100.0) as f32
        }
    }
}

// ---- Monitor brightness over DDC/CI (external monitors) ----

struct Panel {
    handle: HANDLE,
    label: String,
    min: u32,
    max: u32,
}

impl Panel {
    fn get(&self) -> u32 {
        let (mut min, mut cur, mut max) = (0, 0, 0);
        unsafe { GetMonitorBrightness(self.handle, &mut min, &mut cur, &mut max) };
        (cur.saturating_sub(self.min) * 100).checked_div(self.max - self.min).unwrap_or(0)
    }

    fn set(&self, pct: u32) {
        unsafe { SetMonitorBrightness(self.handle, self.min + pct.min(100) * (self.max - self.min) / 100) };
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        let _ = unsafe { DestroyPhysicalMonitor(self.handle) };
    }
}

/// Where a monitor sits relative to the primary one, as its label.
fn side(rc: RECT, primary: RECT) -> &'static str {
    if rc == primary {
        "Utama"
    } else if rc.right <= primary.left {
        "Kiri"
    } else if rc.left >= primary.right {
        "Kanan"
    } else if rc.bottom <= primary.top {
        "Atas"
    } else {
        "Bawah"
    }
}

// ponytail: DDC/CI only; laptop built-in panels need WMI (WmiMonitorBrightnessMethods), add if this runs on one
fn scan_panels() -> Vec<Panel> {
    unsafe extern "system" fn collect(h: HMONITOR, _: HDC, _: *mut RECT, out: LPARAM) -> BOOL {
        unsafe { (*(out.0 as *mut Vec<HMONITOR>)).push(h) };
        TRUE
    }
    let mut monitors: Vec<HMONITOR> = Vec::new();
    let mut out = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut monitors as *mut _ as isize));
        let info = |h: HMONITOR| {
            let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(h, &mut mi);
            mi
        };
        let primary = monitors.iter().map(|&h| info(h)).find(|mi| mi.dwFlags & 1 != 0); // MONITORINFOF_PRIMARY
        let primary = primary.map_or(RECT::default(), |mi| mi.rcMonitor);
        for &h in &monitors {
            let label = side(info(h).rcMonitor, primary);
            let mut n = 0;
            if GetNumberOfPhysicalMonitorsFromHMONITOR(h, &mut n).is_err() || n == 0 {
                continue;
            }
            let mut phys = vec![PHYSICAL_MONITOR::default(); n as usize];
            if GetPhysicalMonitorsFromHMONITOR(h, &mut phys).is_err() {
                continue;
            }
            for pm in phys {
                let (mut min, mut cur, mut max) = (0, 0, 0);
                if GetMonitorBrightness(pm.hPhysicalMonitor, &mut min, &mut cur, &mut max) == 0 || max <= min {
                    let _ = DestroyPhysicalMonitor(pm.hPhysicalMonitor); // no DDC/CI support
                    continue;
                }
                out.push(Panel { handle: pm.hPhysicalMonitor, label: label.into(), min, max });
            }
        }
    }
    // Left-to-right like the physical desk.
    out.sort_by_key(|p| ["Kiri", "Atas", "Utama", "Bawah", "Kanan"].iter().position(|s| *s == p.label));
    out
}

// ---- Media (any app that publishes to Windows media controls: browsers, Spotify, ...) ----

fn media(mgr: &GlobalSystemMediaTransportControlsSessionManager) -> Option<Media> {
    let s = mgr.GetCurrentSession().ok()?;
    let p = s.TryGetMediaPropertiesAsync().ok()?.join().ok()?;
    let title = p.Title().ok()?.to_string();
    if title.is_empty() {
        return None;
    }
    let info = s.GetPlaybackInfo().ok()?;
    let playing = info.PlaybackStatus().ok()? == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing;
    let ctl = info.Controls().ok()?;
    Some(Media {
        title,
        artist: p.Artist().map(|a| a.to_string()).unwrap_or_default(),
        playing,
        can_prev: ctl.IsPreviousEnabled().unwrap_or(false),
        can_next: ctl.IsNextEnabled().unwrap_or(false),
    })
}

// ---- Notifications ----

fn toasts(l: &UserNotificationListener) -> Vec<(u32, String, String, String)> {
    let get = || -> Result<Vec<_>> {
        let generic = KnownNotificationBindings::ToastGeneric()?;
        let mut out = Vec::new();
        for n in l.GetNotificationsAsync(NotificationKinds::Toast)?.join()? {
            let app = n.AppInfo().and_then(|a| a.DisplayInfo()).and_then(|d| d.DisplayName());
            let Ok(texts) = n.Notification()?.Visual()?.GetBinding(&generic).and_then(|b| b.GetTextElements()) else {
                continue;
            };
            let text = |i| texts.GetAt(i).and_then(|t| t.Text()).map(|t| t.to_string()).unwrap_or_default();
            out.push((n.Id()?, app.map(|a| a.to_string()).unwrap_or_default(), text(0), text(1)));
        }
        Ok(out)
    };
    get().unwrap_or_default()
}

// ---- Claude Code ----

fn home() -> std::path::PathBuf {
    std::env::var_os("USERPROFILE").unwrap_or_default().into()
}

fn alive(pid: u32) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut code = 0;
        let ok = GetExitCodeProcess(h, &mut code).is_ok() && code == STILL_ACTIVE.0 as u32;
        let _ = CloseHandle(h);
        ok
    }
}

/// Live Claude Code sessions from ~/.claude/sessions/<pid>.json (written by Claude Code itself).
fn claude_sessions() -> Vec<(String, Session)> {
    let Ok(dir) = std::fs::read_dir(home().join(".claude").join("sessions")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in dir.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(j) = JsonObject::Parse(&HSTRING::from(text)) else { continue };
        let str = |k: &str| j.GetNamedString(&HSTRING::from(k)).map(|s| s.to_string()).unwrap_or_default();
        let pid = j.GetNamedNumber(h!("pid")).unwrap_or(0.0) as u32;
        if !alive(pid) {
            continue; // stale file from a closed session
        }
        let mut name = str("name");
        if name.is_empty() {
            name = str("cwd").rsplit(['\\', '/']).next().unwrap_or_default().to_string();
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
    let text = std::fs::read_to_string(home().join(".claude").join(".credentials.json")).ok()?;
    let o = JsonObject::Parse(&HSTRING::from(text)).ok()?.GetNamedObject(h!("claudeAiOauth")).ok()?;
    Some((o.GetNamedString(h!("accessToken")).ok()?.to_string(), o.GetNamedNumber(h!("expiresAt")).ok()?))
}

// ponytail: relies on `claude auth status` refreshing ~/.claude/.credentials.json (observed, not documented);
// replace with a real OAuth refresh if that ever stops working.
fn refresh_token() {
    let _ = std::process::Command::new("claude")
        .args(["auth", "status"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output();
}

fn claude_usage() -> std::result::Result<Usage, String> {
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
fn fetch_usage(token: &str) -> std::result::Result<Usage, i32> {
    let run = || -> Result<std::result::Result<Usage, i32>> {
        let req = HttpRequestMessage::Create(
            &HttpMethod::Get()?,
            &Uri::CreateUri(h!("https://api.anthropic.com/api/oauth/usage"))?,
        )?;
        let headers = req.Headers()?;
        headers.TryAppendWithoutValidation(h!("Authorization"), &HSTRING::from(format!("Bearer {token}")))?;
        headers.TryAppendWithoutValidation(h!("anthropic-beta"), h!("oauth-2025-04-20"))?;
        let resp = HttpClient::new()?.SendRequestAsync(&req)?.join()?;
        let status = resp.StatusCode()?;
        if status != HttpStatusCode::Ok {
            return Ok(Err(status.0));
        }
        let j = JsonObject::Parse(&resp.Content()?.ReadAsStringAsync()?.join()?)?;
        // `limits` is what the Claude app's usage card renders, per plan (Pro: session + weekly_all,
        // Team: session + weekly_scoped per model), so mirror it as-is.
        let mut out = Vec::new();
        for v in j.GetNamedArray(h!("limits"))? {
            let Ok(o) = v.GetObject() else { continue };
            let kind = o.GetNamedString(h!("kind")).map(|s| s.to_string()).unwrap_or_default();
            let model = o
                .GetNamedObject(h!("scope"))
                .and_then(|s| s.GetNamedObject(h!("model")))
                .and_then(|m| m.GetNamedString(h!("display_name")))
                .map(|s| s.to_string())
                .ok();
            out.push(Limit {
                label: limit_label(&kind, model.as_deref()),
                pct: o.GetNamedNumber(h!("percent")).unwrap_or(0.0),
                reset: o.GetNamedString(h!("resets_at")).ok().and_then(|s| parse_iso(&s.to_string())),
            });
        }
        Ok(Ok(out))
    };
    run().unwrap_or(Err(0))
}
