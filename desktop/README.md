# voicecode desktop (Tauri)

Cross-platform version of voicecode for **Windows, macOS, and Linux (X11)**. Hold the
push-to-talk key, speak, release: each final transcript is typed into the focused window.
Same behavior and settings file as the Python client in the repo root.

## Install

Download the installer for your OS from the latest CI run (artifact `voicecode-desktop-<OS>`):
`.msi`/`.exe` (Windows), `.dmg` (macOS), `.AppImage`/`.deb` (Linux).

Then set the API key and the default endpoint as environment variables:

| OS | How |
|----|-----|
| Windows | `setx VOICECODE_API_KEY "<key>"` and `setx VOICECODE_ENDPOINT "wss://<host>/<path>"`, then sign out and in |
| macOS / Linux | `export VOICECODE_API_KEY=…` and `export VOICECODE_ENDPOINT=…` in `~/.zshrc` or `~/.bashrc` |

On macOS and Linux the app reads them from your login shell, so starting it from Finder,
the Dock, or a file manager works. The endpoint can also be set in **Settings…**.

### macOS permissions

The builds are not signed yet, so the first start needs right-click → **Open**. Then allow:

- **Microphone** (asked on first recording)
- **Accessibility** and **Input Monitoring** (System Settings → Privacy & Security), to read the
  push-to-talk key and type text. Restart voicecode after granting them.

## Use

| Tray menu | What it does |
|-----------|--------------|
| Paused | the key does nothing while checked |
| Language | English (en-US), Chinese (zh-CN), Auto-detect |
| Push-to-talk key | Right Ctrl, Right Alt / Option, Right Shift, F9 |
| Settings… | endpoint URL and vocabulary (one phrase per line) |
| Open log | startup, sessions, errors; never transcript text or the key |

Settings are saved to `<config dir>/voicecode/config.json` (`~/.config/voicecode` on Linux,
shared with the Python client). Only one copy runs at a time.

## Develop

Needs Rust (stable) and, on Linux, the Tauri system packages:

```sh
sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev \
                 libasound2-dev libxdo-dev libx11-dev
cd desktop/src-tauri
cargo test                                  # config, resampler, session vs. a fake server
cargo run                                   # run the app
npx @tauri-apps/cli@2 build                 # installers in target/release/bundle/ (run in desktop/)
```

| Path | Contents |
|------|----------|
| `src-tauri/src/session.rs` | WebSocket streaming: buffer audio during the handshake, partials, finals, 5 s final timeout |
| `src-tauri/src/audio.rs` | microphone capture (cpal), downmix and resample to 16 kHz 16-bit mono |
| `src-tauri/src/input.rs` | push-to-talk key polling (device_query), typing (enigo) after key release |
| `src-tauri/src/config.rs` | settings file, endpoint validation, vocabulary |
| `src-tauri/src/lib.rs` | tray, overlay, settings window, startup |
| `ui/` | overlay and settings pages (plain HTML, no build step) |

## Limits

- Linux needs an X11 session; Wayland blocks global key reading and typing.
- Builds are unsigned: macOS Gatekeeper and Windows SmartScreen warn on first start.
