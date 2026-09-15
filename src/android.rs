//! Android entry point (native-activity). Built into the `cdylib` for the APK.

use android_activity::AndroidApp;

/// Where Android stores per-app files (`/data/data/<pkg>/files`).
fn data_dir(app: &AndroidApp) -> std::path::PathBuf {
    app.internal_data_path()
        .or_else(|| app.external_data_path())
        .unwrap_or_else(|| std::path::PathBuf::from("/data/local/tmp"))
}

/// Native-activity main loop entry point. Called by `android-activity` on a
/// dedicated thread after the Activity's `onCreate`.
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    // Config + SQLite live in the app's private data dir (no writable cwd on Android).
    let dir = data_dir(&app).join("dns_lab");
    let _ = std::fs::create_dir_all(&dir);
    let config_path = dir.join("dns_lab.toml").to_string_lossy().to_string();

    if !std::path::Path::new(&config_path).exists() {
        let mut cfg = crate::config::Config::default();
        cfg.display_name = "android".to_string();
        // Public-DNS tunneling is the sensible default on a phone.
        cfg.egress = crate::config::EgressKind::Resolver;
        cfg.resolver = Some("1.1.1.1:53".to_string());
        // Phones need a larger-than-desktop scale for comfortable touch/reading.
        cfg.ui_scale = 1.4;
        let _ = cfg.save(&config_path);
    }

    let options = crate::app::android_options(app.clone());
    let path = config_path.clone();

    let mut state = crate::app::App::new(path);
    // Gives the app the density scale and the window insets (status/nav bars).
    state.set_android_app(app);

    let _ = eframe::run_native(
        "DNS Messenger",
        options,
        Box::new(move |_cc| Ok(Box::new(state))),
    );
}
