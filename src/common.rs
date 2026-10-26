// Platform-neutral pieces shared by the Windows and Linux builds: the state the collector
// fills and the UI draws, commands from the UI, and small pure helpers (with tests).
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct Media {
    pub title: String,
    pub artist: String,
    pub playing: bool,
    pub can_prev: bool,
    pub can_next: bool,
}

#[derive(Clone)]
pub struct Session {
    pub name: String,
    pub busy: bool,
}

#[derive(Clone)]
pub struct Limit {
    pub label: String,
    pub pct: f64,
    pub reset: Option<i64>,
}

pub type Usage = Vec<Limit>;

#[derive(Default)]
pub struct State {
    pub cpu: f32,
    pub ram: f32,
    pub gpu: f32,
    pub media: Option<Media>,
    pub sessions: Vec<Session>,
    pub usage: Option<Result<Usage, String>>, // None = still loading
    pub notifs: Vec<(u32, String, String)>,   // (notification id or 0, head, body), newest first
    pub flash_at: Option<Instant>,            // when the newest notif arrived
    pub displays: Vec<Display>,               // adjustable monitors, same order as Cmd::Brightness index
}

#[derive(Clone)]
pub struct Display {
    pub label: String,
    pub pct: u32,
}

pub const MAX_NOTIFS: usize = 10;

pub enum Cmd {
    Prev,
    Toggle,
    Next,
    /// Clear a notification from the system notification center too (id 0 = island-only).
    Dismiss(u32),
    /// (display index, brightness 0..=100)
    Brightness(usize, u32),
}

pub fn limit_label(kind: &str, model: Option<&str>) -> String {
    match (kind, model) {
        ("session", _) => "Sesi 5 jam".into(),
        ("weekly_all", _) => "Mingguan".into(),
        ("weekly_scoped", Some(m)) => format!("Mingguan · {m}"),
        (k, _) => k.replace('_', " "),
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

/// "5h 12j" / "2j 30m" / "12m" (hari / jam / menit)
pub fn until(ts: Option<i64>) -> String {
    let Some(ts) = ts else { return "-".into() };
    let s = (ts - now_secs()).max(0);
    let (d, h, m) = (s / 86400, s % 86400 / 3600, s % 3600 / 60);
    if d > 0 { format!("{d}h {h}j") } else if h > 0 { format!("{h}j {m}m") } else { format!("{m}m") }
}

/// "2026-10-31T06:00:00.123+07:00" / "...Z" -> unix seconds.
pub fn parse_iso(s: &str) -> Option<i64> {
    let n = |a: usize, b: usize| -> Option<i64> { s.get(a..b)?.parse().ok() };
    let (y, mo, d, h, mi, se) = (n(0, 4)?, n(5, 7)?, n(8, 10)?, n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let tail = s.get(19..)?;
    let off = match tail.rfind(['+', '-']) {
        Some(i) => {
            let t = &tail[i..];
            let sign = if t.starts_with('-') { -1 } else { 1 };
            sign * (t.get(1..3)?.parse::<i64>().ok()? * 3600 + t.get(4..6)?.parse::<i64>().ok()? * 60)
        }
        None => 0,
    };
    // days from civil (Howard Hinnant)
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + se - off)
}

#[cfg(test)]
mod tests {
    use super::{limit_label, parse_iso};
    #[test]
    fn labels() {
        assert_eq!(limit_label("session", None), "Sesi 5 jam");
        assert_eq!(limit_label("weekly_scoped", Some("Fable")), "Mingguan · Fable");
        assert_eq!(limit_label("weekly_new_thing", None), "weekly new thing");
    }
    #[test]
    fn iso() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2000-03-01T00:00:00+00:00"), Some(951868800));
        assert_eq!(parse_iso("1970-01-01T07:00:00.5+07:00"), Some(0));
        assert_eq!(parse_iso("2026-10-31T06:00:00+00:00"), Some(1793426400));
        assert_eq!(parse_iso("garbage"), None);
    }
}
