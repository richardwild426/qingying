//! Runs the Rust application directly on the machine's native backend.

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
use nsh_platform_linux as platform;
#[cfg(target_os = "macos")]
use nsh_platform_macos as platform;
#[cfg(target_os = "windows")]
use nsh_platform_windows as platform;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn main() -> impl std::process::Termination {
    // A .app launched from Finder has no terminal: keep a log file so a
    // misbehaving picker or encoder can be diagnosed after the fact.
    // `std::env::temp_dir()` points inside the app's container there, so the
    // log goes to a predictable path under $HOME instead.
    let log_path = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("qingying.log");
    let mut builder = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    );
    if let Ok(file) = std::fs::File::create(&log_path) {
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    }
    builder.init();
    log::info!("--- started, logging to {} ---", log_path.display());
    platform::run_app(
        platform::AppSource::Rust(platform::Root::of::<nsh_app_video_squeezer::App>()),
        platform::AppOptions {
            title: "轻影".to_owned(),
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            app_id: "com.example.videosqueezer".to_owned(),
            size: (560, 560),
            ..Default::default()
        },
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!(
    "Run this binary on macOS, Windows or Linux; use the TSX entry with nsh run for iOS and Android."
);
