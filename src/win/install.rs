// Self-installer: the same exe, renamed FloatIsland-Setup-*.exe, installs itself per-user
// (no admin, no installer toolkit). The installed copy registers autostart and an
// "Installed apps" entry whose uninstall runs `FloatIsland.exe --uninstall`.
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::time::Duration;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

const NAME: &str = "FloatIsland";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const UNINSTALL_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\FloatIsland");

fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var).unwrap_or_default().into()
}

fn install_dir() -> PathBuf {
    env_dir("LOCALAPPDATA").join("Programs").join(NAME)
}

fn installed_exe() -> PathBuf {
    install_dir().join(format!("{NAME}.exe"))
}

fn shortcut() -> PathBuf {
    env_dir("APPDATA").join("Microsoft\\Windows\\Start Menu\\Programs").join(format!("{NAME}.lnk"))
}

fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_default()
}

pub fn is_installed_copy() -> bool {
    let same = |a: PathBuf, b: PathBuf| a.canonicalize().ok().is_some_and(|a| Some(a) == b.canonicalize().ok());
    same(current_exe(), installed_exe())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn set_str(key: PCWSTR, name: &str, value: &str) {
    let v = wide(value);
    let n = HSTRING::from(name);
    let _ = unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, key, &n, REG_SZ.0, Some(v.as_ptr() as *const _), (v.len() * 2) as u32)
    };
}

fn set_dword(key: PCWSTR, name: &str, value: u32) {
    let n = HSTRING::from(name);
    let _ = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, key, &n, REG_DWORD.0, Some(&value as *const u32 as *const _), 4) };
}

const SETTINGS_KEY: PCWSTR = w!("Software\\FloatIsland");

/// Saved island position: true = bottom (taskbar), false = top. Defaults to top.
pub fn load_position() -> bool {
    let (mut v, mut size) = (0u32, 4u32);
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            SETTINGS_KEY,
            w!("Position"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    ok.is_ok() && v == 1
}

pub fn save_position(bottom: bool) {
    set_dword(SETTINGS_KEY, "Position", bottom as u32);
}

/// Launch at login via HKCU\...\Run. Turning it off in Task Manager > Startup apps still wins
/// (Windows keeps that flag separately).
pub fn register_autostart() {
    set_str(RUN_KEY, NAME, &format!("\"{}\"", current_exe().display()));
}

fn message(text: &str, icon: MESSAGEBOX_STYLE) {
    let (t, c) = (HSTRING::from(text), HSTRING::from(NAME));
    unsafe { MessageBoxW(None, &t, &c, MB_OK | icon) };
}

/// Ask a running island to quit and wait for it, so its exe can be replaced or deleted.
fn stop_running() {
    unsafe {
        for _ in 0..50 {
            let Ok(hwnd) = FindWindowW(w!("FloatIsland"), None) else { return };
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn create_shortcut(target: &PathBuf) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(target.as_os_str()))?;
        link.SetDescription(w!("FloatIsland"))?;
        link.cast::<IPersistFile>()?.Save(&HSTRING::from(shortcut().as_os_str()), true)
    }
}

fn install() {
    stop_running();
    let target = installed_exe();
    let copied = std::fs::create_dir_all(install_dir()).and_then(|_| std::fs::copy(current_exe(), &target));
    if let Err(e) = copied {
        message(&format!("Gagal memasang {NAME}:\n{e}"), MB_ICONERROR);
        return;
    }
    let _ = create_shortcut(&target);
    let exe = target.display().to_string();
    set_str(UNINSTALL_KEY, "DisplayName", NAME);
    set_str(UNINSTALL_KEY, "DisplayVersion", VERSION);
    set_str(UNINSTALL_KEY, "Publisher", NAME);
    set_str(UNINSTALL_KEY, "DisplayIcon", &exe);
    set_str(UNINSTALL_KEY, "InstallLocation", &install_dir().display().to_string());
    set_str(UNINSTALL_KEY, "UninstallString", &format!("\"{exe}\" --uninstall"));
    set_dword(UNINSTALL_KEY, "NoModify", 1);
    set_dword(UNINSTALL_KEY, "NoRepair", 1);
    set_dword(UNINSTALL_KEY, "EstimatedSize", (std::fs::metadata(&target).map_or(0, |m| m.len()) / 1024) as u32);

    let _ = std::process::Command::new(&target).spawn(); // the installed copy registers autostart itself
    message(
        &format!("{NAME} {VERSION} terpasang.\n\nBerjalan otomatis saat login. Klik kanan island untuk keluar."),
        MB_ICONINFORMATION,
    );
}

fn uninstall() {
    stop_running();
    unsafe {
        let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("FloatIsland"));
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, UNINSTALL_KEY);
    }
    let _ = std::fs::remove_file(shortcut());
    // This exe is the one being deleted, so a detached cmd removes the folder once we've exited,
    // retrying for ~10 s because the exe can stay locked briefly (our exit, antivirus scan).
    let dir = install_dir().display().to_string();
    let _ = std::process::Command::new("cmd")
        .raw_arg(format!(
            "/c for /l %i in (1,1,10) do (ping -n 2 127.0.0.1 >nul & rmdir /s /q \"{dir}\" & if not exist \"{dir}\" exit)"
        ))
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .spawn();
    message(&format!("{NAME} sudah dihapus."), MB_ICONINFORMATION);
}

/// Handles `--uninstall` and the Setup-named exe. Returns true when the process should exit.
pub fn setup_or_uninstall() -> bool {
    if std::env::args().any(|a| a == "--uninstall") {
        uninstall();
        return true;
    }
    let stem = current_exe().file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    if stem.starts_with("floatisland-setup") {
        install();
        return true;
    }
    false
}
