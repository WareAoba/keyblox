// Embed a Common-Controls v6 manifest (modern-looking file dialog) on Windows.
fn main() {
    println!("cargo:rerun-if-changed=res/app.rc");
    println!("cargo:rerun-if-changed=res/app.manifest");
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    if target.contains("msvc") {
        // MSVC link.exe embeds the manifest itself
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{dir}\\res\\app.manifest");
        return;
    }
    // GNU toolchain: compile res/app.rc with windres
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("app_res.o");
    let windres = if cfg!(windows) { "windres" } else { "x86_64-w64-mingw32-windres" };
    let ok = std::process::Command::new(windres)
        .args(["-I", "res", "-O", "coff", "res/app.rc", "-o"])
        .arg(&out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        println!("cargo:rustc-link-arg-bins={}", out.display());
    } else {
        println!("cargo:warning=windres not found: building without manifest");
    }
}
