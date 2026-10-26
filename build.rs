// Embeds the logo and the "FloatIsland" name (shown in Task Manager / Startup apps) into the
// Windows exe. Linux builds use assets/logo.png through the .desktop file instead.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico")
        .set("FileDescription", "FloatIsland")
        .set("ProductName", "FloatIsland")
        .set("OriginalFilename", "FloatIsland.exe");
    res.compile().expect("failed to embed Windows resources");
}
