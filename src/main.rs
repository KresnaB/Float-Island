#![cfg_attr(windows, windows_subsystem = "windows")]
// FloatIsland: a Dynamic Island-style status pill. One codebase, two native front ends:
// src/win (Win32 + Direct2D) and src/linux (GTK4 + layer-shell / XWayland), sharing src/common.
mod common;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod win;

fn main() {
    #[cfg(windows)]
    let _ = win::main();
    #[cfg(target_os = "linux")]
    linux::main();
}
