// Compiles the Slint UI. Widgets use the platform's own look: Cupertino on macOS,
// Fluent on Windows and Linux (Slint's "native" style would need Qt installed).
fn main() {
    #[cfg(windows)]
    windows_icon();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let style = if target_os == "macos" { "cupertino" } else { "fluent" };
    let config = slint_build::CompilerConfiguration::new().with_style(style.into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI failed to compile");
}

// Windows: give the .exe the app icon (Explorer, taskbar, installer).
#[cfg(windows)]
fn windows_icon() {
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    let _ = res.compile();
}
