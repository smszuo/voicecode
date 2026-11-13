# voicecode

Push-to-talk dictation for Linux X11. Hold the push-to-talk key (default **Right Ctrl**),
speak, release. Each final transcript from the streaming speech-to-text service is typed into the
focused window. A tray icon shows the state and holds the settings.

## Setup

```sh
sudo apt install xdotool gir1.2-ayatanaappindicator3-0.1
export VOICECODE_API_KEY=<project-api-key>   # e.g. in ~/.zshrc
export VOICECODE_ENDPOINT=wss://<host>/<path>     # default endpoint; tray can override

# GTK bindings come from system Python, so the venv must see system packages.
uv venv --python /usr/bin/python3 --system-site-packages .venv
uv pip install --python .venv/bin/python -e .
```

## Run

```sh
.venv/bin/voicecode          # or: .venv/bin/python -m voicecode
```

## Settings (tray menu)

| Item | Choices |
|------|---------|
| Paused | on / off — the key does nothing while paused |
| Language | English (en-US), Chinese (zh-CN), Auto-detect |
| Push-to-talk key | Right Ctrl, Right Alt, F9, Pause |
| Endpoint URL… | overrides `$VOICECODE_ENDPOINT`; `wss://` only (`ws://` for localhost); Reset goes back to the env value |
| Edit vocabulary… | opens `~/.config/voicecode/vocabulary.txt`, one phrase per line |
| Open log | opens `~/.cache/voicecode.log` (startup, sessions, errors; never transcript text) |

Changes apply right away and are saved to `~/.config/voicecode/config.json`.
The API key is never stored there; it stays in the environment.

Tray icon: microphone = idle, loud microphone = recording, crossed-out = paused.

## Start at login (GNOME)

Runs the AppImage, so build it first (next section).

```sh
mkdir -p ~/.config/autostart
sed "s|@APPIMAGE@|$PWD/dist/voicecode-x86_64.AppImage|" packaging/voicecode-autostart.desktop \
    > ~/.config/autostart/voicecode.desktop                                   # remove to turn off
gtk-launch voicecode                                                         # start it now
```

## AppImage (double-click to run)

```sh
sudo apt install desktop-file-utils   # appimagetool needs desktop-file-validate
scripts/build-appimage.sh    # downloads appimagetool 1.9.0 once
# -> dist/voicecode-<version>-x86_64.AppImage, plus dist/voicecode-x86_64.AppImage pointing to it
# The version comes from pyproject.toml; bump it there before a release.
```

It is a thin AppImage: it bundles the app, `websockets`, and `python-xlib`, and uses the
system `/usr/bin/python3` with apt's GTK and tray packages (see Setup). On double-click it
loads `VOICECODE_API_KEY` / `VOICECODE_ENDPOINT` through your login shell's rc file. Startup errors
show as a dialog. Only one copy runs at a time.

## Project layout

```
src/voicecode/        app code (app.py); `python -m voicecode` runs it
tests/                self-test: config handling + fake local server
packaging/            AppImage files (AppRun, .desktop, icon) and the autostart .desktop
scripts/              build-appimage.sh
.github/workflows/    ci.yml: tests on Python 3.10 + 3.12, then builds the AppImage as an artifact
pyproject.toml        package metadata, dependencies, `voicecode` command
```

## Test

```sh
.venv/bin/python tests/test_voicecode.py   # config handling + fake local server, no API calls
```
