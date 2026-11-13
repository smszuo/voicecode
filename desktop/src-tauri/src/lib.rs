//! voicecode desktop: push-to-talk dictation with a tray menu, for Windows, macOS, and Linux X11.

mod audio;
mod config;
mod input;
mod session;

use config::{Config, ENDPOINT_ENV, KEY_ENV, LANGUAGES, PTT_KEYS};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, RunEvent, WebviewUrl, WebviewWindowBuilder, Wry,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;

const ERROR_SHOW: Duration = Duration::from_secs(3);
const OVERLAY_W: f64 = 720.0;
const OVERLAY_H: f64 = 96.0;
const TRAY_IDLE: &[u8] = include_bytes!("../icons/tray-idle.png");
const TRAY_RECORDING: &[u8] = include_bytes!("../icons/tray-recording.png");
const TRAY_PAUSED: &[u8] = include_bytes!("../icons/tray-paused.png");

struct AppState {
    cfg: Mutex<Config>,
    key: String,
    env_endpoint: String,
    ptt_down: Arc<AtomicBool>,
    /// Queue of the single typing worker (see input::spawn_typer).
    typer: std_mpsc::Sender<String>,
    recording: AtomicBool,
    /// Bumped on every session start, so an old error's hide timer cannot hide a new session.
    generation: AtomicU64,
    tray: Mutex<Option<TrayIcon>>,
    /// (setting, value, item) for the radio-style submenus.
    choices: Mutex<Vec<(&'static str, &'static str, CheckMenuItem<Wry>)>>,
}

// ---------- overlay ----------

/// UI of one session. Only the newest session may change the overlay and tray: an older one
/// that is still finishing must not hide the overlay of a new recording.
struct OverlayUi(AppHandle, u64);

impl OverlayUi {
    fn current(app: &AppHandle) -> Self {
        OverlayUi(
            app.clone(),
            app.state::<AppState>().generation.load(Ordering::Relaxed),
        )
    }
    fn is_current(&self) -> bool {
        self.0
            .state::<AppState>()
            .generation
            .load(Ordering::Relaxed)
            == self.1
    }
    fn emit(&self, kind: &str, text: &str) {
        if !self.is_current() {
            return;
        }
        let _ = self.0.emit_to(
            "overlay",
            "overlay",
            serde_json::json!({ "kind": kind, "text": text }),
        );
    }
    fn set_visible(&self, visible: bool) {
        if !self.is_current() {
            return;
        }
        if let Some(w) = self.0.get_webview_window("overlay") {
            if visible {
                let _ = w.show();
                // Click-through. Only after show(): on Linux the native window does not exist before.
                let _ = w.set_ignore_cursor_events(true);
            } else {
                let _ = w.hide();
            }
        }
    }
    fn stop_recording(&self) {
        if !self.is_current() {
            return;
        }
        self.0
            .state::<AppState>()
            .recording
            .store(false, Ordering::Relaxed);
        update_tray_icon(&self.0);
    }
}

impl session::Ui for OverlayUi {
    fn partial(&self, text: &str) {
        self.emit("partial", text);
    }
    fn final_text(&self, text: &str) {
        self.emit("final", text);
    }
    fn error(&self, text: &str) {
        self.stop_recording();
        self.emit("error", text);
        self.set_visible(true);
        let ui = OverlayUi(self.0.clone(), self.1);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(ERROR_SHOW).await;
            ui.set_visible(false); // no-op if a newer session started meanwhile
        });
    }
    fn hide(&self) {
        self.stop_recording();
        self.set_visible(false);
    }
}

fn create_overlay(app: &AppHandle) -> tauri::Result<()> {
    let w = WebviewWindowBuilder::new(app, "overlay", WebviewUrl::App("overlay.html".into()))
        .title("voicecode")
        .inner_size(OVERLAY_W, OVERLAY_H)
        .decorations(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        // Never take focus: text must keep going to the window the user is typing in.
        .focused(false)
        .focusable(false)
        .build()?;
    if let Some(m) = w.primary_monitor()? {
        let scale = m.scale_factor();
        let (mw, mh) = (m.size().width as f64, m.size().height as f64);
        let x = m.position().x as f64 + (mw - OVERLAY_W * scale) / 2.0;
        let y = m.position().y as f64 + mh - (OVERLAY_H + 80.0) * scale;
        w.set_position(PhysicalPosition::new(x as i32, y as i32))?;
    }
    Ok(())
}

// ---------- sessions ----------

/// Starts recording and streaming; send on (or drop) the returned sender to end the audio.
fn start_session(app: &AppHandle) -> std_mpsc::Sender<()> {
    let state = app.state::<AppState>();
    state.generation.fetch_add(1, Ordering::Relaxed);
    state.recording.store(true, Ordering::Relaxed);
    update_tray_icon(app);
    let ui = OverlayUi::current(app);
    ui.emit("start", "");
    ui.set_visible(true);

    let cfg = state.cfg.lock().unwrap().clone();
    let url = config::stream_url(
        &cfg.language,
        &config::resolve_endpoint(&cfg, &state.env_endpoint),
    );
    let key = state.key.clone();
    let typer = state.typer.clone();
    let vocabulary = config::load_vocabulary(&config::vocab_file());
    let (stop_tx, stop_rx) = std_mpsc::channel();
    let (audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = audio::record(audio_tx, stop_rx).await {
            log::warn!("{e}");
            session::Ui::error(&ui, &e);
            return;
        }
        session::run_session(
            audio_rx,
            &ui,
            &key,
            &url,
            &vocabulary,
            session::FINAL_TIMEOUT,
            &typer,
        )
        .await;
    });
    stop_tx
}

fn watch_ptt_key(app: AppHandle) {
    let Some(poller) = input::KeyPoller::new() else {
        let msg = if cfg!(target_os = "macos") {
            "Cannot read the keyboard. Allow voicecode in System Settings > Privacy & Security > \
             Accessibility and Input Monitoring, then restart it."
        } else {
            "Cannot read the keyboard (no X11 display?). Wayland sessions are not supported."
        };
        log::error!("{msg}");
        session::Ui::error(&OverlayUi::current(&app), msg);
        return;
    };
    let state = app.state::<AppState>();
    let mut stop: Option<std_mpsc::Sender<()>> = None;
    loop {
        let (key_name, paused) = {
            let c = state.cfg.lock().unwrap();
            (c.ptt_key.clone(), c.paused)
        };
        let down = input::keycode(&key_name).is_some_and(|k| poller.is_down(k));
        state.ptt_down.store(down, Ordering::Relaxed);
        if down && stop.is_none() && !paused {
            stop = Some(start_session(&app));
        } else if !down && let Some(s) = stop.take() {
            let _ = s.send(());
        }
        std::thread::sleep(input::POLL);
    }
}

// ---------- tray ----------

fn update_tray_icon(app: &AppHandle) {
    let state = app.state::<AppState>();
    let bytes = if state.cfg.lock().unwrap().paused {
        TRAY_PAUSED
    } else if state.recording.load(Ordering::Relaxed) {
        TRAY_RECORDING
    } else {
        TRAY_IDLE
    };
    if let (Some(tray), Ok(icon)) = (
        state.tray.lock().unwrap().as_ref(),
        Image::from_bytes(bytes),
    ) {
        let _ = tray.set_icon(Some(icon));
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let state = app.state::<AppState>();
    let cfg = state.cfg.lock().unwrap().clone();
    let version = MenuItem::with_id(
        app,
        "version",
        format!("voicecode {}", app.package_info().version),
        false,
        None::<&str>,
    )?;
    let paused = CheckMenuItem::with_id(app, "paused", "Paused", true, cfg.paused, None::<&str>)?;

    let mut choices = Vec::new();
    let mut submenu = |title: &str,
                       setting: &'static str,
                       options: &'static [(&'static str, &'static str)],
                       current: &str| {
        let items = options
            .iter()
            .map(|(label, value)| {
                let item = CheckMenuItem::with_id(
                    app,
                    format!("{setting}:{value}"),
                    *label,
                    true,
                    current == *value,
                    None::<&str>,
                )?;
                choices.push((setting, *value, item.clone()));
                Ok(item)
            })
            .collect::<tauri::Result<Vec<_>>>()?;
        let refs: Vec<&dyn IsMenuItem<Wry>> =
            items.iter().map(|i| i as &dyn IsMenuItem<Wry>).collect();
        Submenu::with_items(app, title, true, &refs)
    };
    let language = submenu("Language", "language", LANGUAGES, &cfg.language)?;
    let ptt_key = submenu("Push-to-talk key", "ptt_key", PTT_KEYS, &cfg.ptt_key)?;
    *state.choices.lock().unwrap() = choices;

    let menu = Menu::with_items(
        app,
        &[
            &version,
            &PredefinedMenuItem::separator(app)?,
            &paused,
            &language,
            &ptt_key,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?,
            &MenuItem::with_id(app, "open_log", "Open log", true, None::<&str>)?,
            &MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?,
        ],
    )?;
    let tray = TrayIconBuilder::with_id("main")
        .icon(Image::from_bytes(TRAY_IDLE)?)
        .tooltip("voicecode")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(on_menu)
        .build(app)?;
    *state.tray.lock().unwrap() = Some(tray);
    update_tray_icon(app);
    Ok(())
}

fn on_menu(app: &AppHandle, event: MenuEvent) {
    let state = app.state::<AppState>();
    let id = event.id().as_ref();
    match id {
        "settings" => open_settings(app),
        "open_log" => {
            if let Ok(dir) = app.path().app_log_dir()
                && let Err(e) = app
                    .opener()
                    .open_path(dir.join("voicecode.log").to_string_lossy(), None::<&str>)
            {
                log::warn!("open log: {e}");
            }
        }
        "quit" => app.exit(0),
        "paused" => {
            let mut cfg = state.cfg.lock().unwrap();
            cfg.paused = !cfg.paused;
            save(&cfg);
        }
        _ => {
            let Some((setting, value)) = id.split_once(':') else {
                return;
            };
            {
                let mut cfg = state.cfg.lock().unwrap();
                match setting {
                    "language" => cfg.language = value.into(),
                    "ptt_key" => cfg.ptt_key = value.into(),
                    _ => return,
                }
                save(&cfg);
            }
            // Radio behavior: exactly one checked item per submenu.
            for (s, v, item) in state.choices.lock().unwrap().iter() {
                if *s == setting {
                    let _ = item.set_checked(*v == value);
                }
            }
        }
    }
    update_tray_icon(app);
}

fn save(cfg: &Config) {
    if let Err(e) = config::save_config(cfg, &config::config_file()) {
        log::error!("saving settings failed: {e}");
    }
}

// ---------- settings window ----------

fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title("voicecode settings")
        .inner_size(560.0, 560.0)
        .build();
    if let Err(e) = built {
        log::error!("settings window: {e}");
    }
}

#[derive(Serialize)]
struct SettingsView {
    endpoint: String,
    env_endpoint: String,
    endpoint_env_name: &'static str,
    vocabulary: String,
    version: String,
}

#[tauri::command]
fn get_settings(app: AppHandle, state: tauri::State<AppState>) -> SettingsView {
    SettingsView {
        endpoint: state.cfg.lock().unwrap().endpoint.clone(),
        env_endpoint: state.env_endpoint.clone(),
        endpoint_env_name: ENDPOINT_ENV,
        vocabulary: std::fs::read_to_string(config::vocab_file()).unwrap_or_default(),
        version: app.package_info().version.to_string(),
    }
}

#[tauri::command]
fn save_settings(
    state: tauri::State<AppState>,
    endpoint: String,
    vocabulary: String,
) -> Result<(), String> {
    let endpoint = endpoint.trim();
    if !endpoint.is_empty() && !config::valid_endpoint(endpoint) {
        return Err("Not a valid URL. Use wss://host/path (ws:// only for localhost).".into());
    }
    if endpoint.is_empty() && !config::valid_endpoint(&state.env_endpoint) {
        return Err(format!(
            "${ENDPOINT_ENV} is not set, so the endpoint cannot be empty."
        ));
    }
    let path = config::vocab_file();
    std::fs::create_dir_all(config::config_dir()).map_err(|e| e.to_string())?;
    std::fs::write(&path, vocabulary).map_err(|e| format!("Saving vocabulary failed: {e}"))?;
    let mut cfg = state.cfg.lock().unwrap();
    cfg.endpoint = endpoint.to_string();
    config::save_config(&cfg, &config::config_file())
        .map_err(|e| format!("Saving settings failed: {e}"))
}

// ---------- startup ----------

/// Key and default endpoint from the environment. Apps started from Finder, the Dock, or a file
/// manager do not load shell rc files, so on macOS/Linux fall back to asking the login shell.
fn read_env() -> (String, String) {
    let get = |k| std::env::var(k).unwrap_or_default().trim().to_string();
    let (key, endpoint) = (get(KEY_ENV), get(ENDPOINT_ENV));
    #[cfg(unix)]
    if key.is_empty()
        && let Some((k, e)) = login_shell_env()
    {
        return (k, if endpoint.is_empty() { e } else { endpoint });
    }
    (key, endpoint)
}

#[cfg(unix)]
fn login_shell_env() -> Option<(String, String)> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let script = format!("printf '\\n%s\\n%s\\n' \"${KEY_ENV}\" \"${ENDPOINT_ENV}\"");
    let out = std::process::Command::new(shell)
        .args(["-ilc", &script])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // rc files may print text first; our two values are always the last two lines.
    let mut lines = text.lines().rev();
    let endpoint = lines.next()?.trim().to_string();
    let key = lines.next()?.trim().to_string();
    Some((key, endpoint))
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Accessory); // tray only, no Dock icon

    let (key, env_endpoint) = read_env();
    if key.is_empty() {
        let msg = format!(
            "{KEY_ENV} is not set. Set it in your environment, then start voicecode again."
        );
        log::error!("{msg}");
        let handle = app.handle().clone();
        app.dialog()
            .message(msg)
            .title("voicecode cannot start")
            .kind(MessageDialogKind::Error)
            .show(move |_| handle.exit(1));
        return Ok(());
    }
    let cfg = config::load_config(&config::config_file());
    let endpoint = config::resolve_endpoint(&cfg, &env_endpoint);
    let ptt_down = Arc::new(AtomicBool::new(false));
    app.manage(AppState {
        cfg: Mutex::new(cfg),
        key,
        env_endpoint,
        typer: input::spawn_typer(ptt_down.clone()),
        ptt_down,
        recording: AtomicBool::new(false),
        generation: AtomicU64::new(0),
        tray: Mutex::new(None),
        choices: Mutex::new(Vec::new()),
    });

    create_overlay(app.handle())?;
    build_tray(app.handle())?;
    if !config::valid_endpoint(&endpoint) {
        log::warn!("no valid endpoint yet; opening settings");
        open_settings(app.handle());
    }
    let handle = app.handle().clone();
    std::thread::spawn(move || watch_ptt_key(handle));
    log::info!(
        "voicecode {} ready (endpoint {endpoint})",
        app.package_info().version
    );
    Ok(())
}

pub fn run() {
    // rustls 0.23 needs an explicit crypto provider for wss://; Err only means one is already set.
    let _ = rustls::crypto::ring::default_provider().install_default();
    tauri::Builder::default()
        // Two copies would both hear the key and type everything twice. Must be the first plugin.
        .plugin(tauri_plugin_single_instance::init(|_, _, _| {
            log::info!("second start ignored: already running")
        }))
        .plugin(
            tauri_plugin_log::Builder::new()
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("voicecode".into()),
                    }),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                ])
                .level(log::LevelFilter::Info)
                .max_file_size(1_000_000)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepOne)
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![get_settings, save_settings])
        .setup(setup)
        .build(tauri::generate_context!())
        .expect("failed to start voicecode")
        .run(|_, event| {
            // Tray app: closing the settings window must not quit. app.exit() passes a code.
            if let RunEvent::ExitRequested {
                code: None, api, ..
            } = event
            {
                api.prevent_exit();
            }
        });
}
