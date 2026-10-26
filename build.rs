// Embeds the logo and the "FloatIsland" name (shown in Task Manager / Startup apps) into the exe.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico")
        .set("FileDescription", "FloatIsland")
        .set("ProductName", "FloatIsland")
        .set("OriginalFilename", "FloatIsland.exe");
    res.compile().expect("failed to embed Windows resources");
}
