// Windows: put the icon and the version details into forgecode.exe. Nothing to do elsewhere
// (macOS gets its icon from the .app bundle, see scripts/bundle.sh).
fn main() {
    println!("cargo:rerun-if-changed=assets/forgecode.rc");
    println!("cargo:rerun-if-changed=assets/logo.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // The .rc with the version and the icon's full path written in (no quoting through macros).
    let version = env!("CARGO_PKG_VERSION");
    let mut parts: Vec<&str> = version.split(['.', '-', '+']).take(3).collect();
    parts.resize(4, "0");
    let icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join("logo.ico");
    let rc = std::fs::read_to_string("assets/forgecode.rc")
        .expect("assets/forgecode.rc")
        .replace("\"logo.ico\"", &format!("\"{}\"", icon.display().to_string().replace('\\', "\\\\")))
        .replace("VERSION_COMMAS", &parts.join(","))
        .replace("VERSION_TEXT", &format!("\"{version}\""));
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("forgecode.rc");
    std::fs::write(&out, rc).expect("write forgecode.rc");
    embed_resource::compile(&out, embed_resource::NONE).manifest_optional().expect("Windows resources (icon, version)");
}
