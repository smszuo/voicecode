//! Push-to-talk key polling and text typing, both cross-platform.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const POLL: Duration = Duration::from_millis(20);

/// Stored key name (X11 keysym names, shared with the Python client) -> device_query key.
pub fn keycode(name: &str) -> Option<device_query::Keycode> {
    use device_query::Keycode::*;
    Some(match name {
        "Control_R" => RControl,
        "Alt_R" if cfg!(target_os = "macos") => ROption,
        "Alt_R" => RAlt,
        "Shift_R" => RShift,
        "F9" => F9,
        _ => return None,
    })
}

/// Polls the keyboard state (no event hooks, so no key is ever swallowed or delayed).
pub struct KeyPoller(device_query::DeviceState);

impl KeyPoller {
    /// None when the platform refuses (no X display, or macOS without the permission).
    pub fn new() -> Option<Self> {
        device_query::DeviceState::checked_new().map(KeyPoller)
    }

    pub fn is_down(&self, key: device_query::Keycode) -> bool {
        use device_query::DeviceQuery;
        self.0.get_keys().contains(&key)
    }
}

/// Starts the one typing worker. Texts sent to the returned queue are typed in order into the
/// focused window, each once the push-to-talk key is up: typing while Ctrl is held would turn
/// letters into shortcuts. Typing is slow on X11 (per character), so it must not block a session.
pub fn spawn_typer(ptt_down: Arc<AtomicBool>) -> std::sync::mpsc::Sender<String> {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use enigo::{Enigo, Keyboard, Settings};
        let mut enigo = None;
        for text in rx {
            wait_for_release(&ptt_down);
            if enigo.is_none() {
                enigo = Enigo::new(&Settings::default())
                    .map_err(|e| log::warn!("typing unavailable: {e}"))
                    .ok();
            }
            let Some(e) = enigo.as_mut() else { continue };
            let started = std::time::Instant::now();
            match e.text(&text) {
                Ok(()) => log::info!(
                    "typed {} chars in {} ms",
                    text.chars().count(),
                    started.elapsed().as_millis()
                ),
                Err(err) => log::warn!("typing failed: {err}"),
            }
        }
    });
    tx
}

fn wait_for_release(ptt_down: &AtomicBool) {
    while ptt_down.load(Ordering::Relaxed) {
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_while_key_held() {
        let down = Arc::new(AtomicBool::new(true));
        let d = down.clone();
        let wait = std::thread::spawn(move || wait_for_release(&d));
        std::thread::sleep(Duration::from_millis(200));
        assert!(!wait.is_finished(), "must not type while the key is held");
        down.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(100));
        assert!(wait.is_finished(), "types right after release");
    }

    #[test]
    fn every_tray_key_maps() {
        for (_, name) in crate::config::PTT_KEYS {
            assert!(keycode(name).is_some(), "{name}");
        }
        assert!(keycode("Nope").is_none());
    }
}
