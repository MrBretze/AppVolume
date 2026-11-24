# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

`appvolume` is an OpenAction plugin for [OpenDeck](https://github.com/nekename/OpenDeck): each Stream Deck+ dial controls one application's volume (volume mode) or output device (output mode, entered with a long press). It is a single Rust binary (edition 2024) built on the `openaction` crate and `tokio`. User-facing behaviour is described in `README.md`.

Code, comments and docs are written in English.

## Commands

- Build: `cargo build`
- Test: `cargo test`, or a single test with `cargo test wraps_long_device_names`
- Lint: `cargo clippy --all-targets`, and for the other platforms `cargo clippy --all-targets --target x86_64-pc-windows-gnu` and `--target aarch64-apple-darwin` (both targets are installed; clippy needs no linker). The Windows backend can only be compiled, not run, here.
- CI (`.github/workflows/release.yml`) runs `cargo test --locked` on Linux, Windows and macOS for every push: commit `Cargo.lock` with any dependency change.
- Format: `cargo fmt` (`rustfmt.toml` uses hard tabs)
- Install into the local OpenDeck: `scripts/install.sh`, then restart OpenDeck
- Inspect what the plugin sees without OpenDeck: `cargo run -- --list`
- Debug logs: the "Debug logs" setting in the property inspector (default `config::DEBUG_DEFAULT` in `src/config.rs`, `false` in releases). OpenDeck writes the plugin's stdout to `~/.var/app/me.amankhanna.opendeck/data/opendeck/logs/plugins/com.mrbretzel.appvolume.sdPlugin.log`.

The binary is normally launched by OpenDeck, which passes the connection arguments on to `openaction::run`.

## Architecture

- `src/audio/`: the `AudioBackend` trait (blocking calls) and one implementation per platform, selected with `cfg` in `audio::backend()`.
  - `linux.rs` speaks the PulseAudio native protocol through the pure-Rust `pulseaudio` crate, not libpulse, so the binary has no C dependency. It keeps one request connection and starts one thread with its own connection that receives change events. The client API of that crate lacks sink-input commands, so the module uses `pulseaudio::protocol` directly.
  - `windows.rs` uses WASAPI sessions for volume. For output routing it calls the undocumented `IAudioPolicyConfigFactory` through a raw vtable slot (`SET_PERSISTED_DEFAULT_ENDPOINT_SLOT`); the `windows::core::interface` macro cannot declare an `IInspectable`-based interface.
  - `unsupported.rs` (macOS) returns `AudioError::Unsupported`.
  - Apps are grouped by `AppStream::key`, the lowercase executable name. That key is also what gets persisted in the action settings.
- `src/state.rs`: the global `PLUGIN` holds the latest mixer snapshot, the icon cache, the per-dial `Dial`s and a volume-writer thread that coalesces fast dial turns. `refresh()` re-reads the mixer in `spawn_blocking`. `redraw()` compares the wanted screen with the last one sent (`Dial::sent`) and sends only the changed keys.
- `src/dial.rs`: input handling. A long press is detected by a timer task started on `dial_down`. Turning the dial while it is held cancels both the click and the long press. After a successful output change, `Dial::confirm` makes `state::screen_for` show `layouts/confirm.json` (check frames pre-rendered by `icons::check_frames`), driven by `dial::animate_confirmation`; any dial input cancels it.
- `src/render.rs` builds `setFeedback` payloads for the custom layouts in `assets/layouts/*.json`. **The key names in `render.rs` must match the `key` fields of those layouts.** Switching layouts (`setFeedbackLayout`) resets every item, which is why `redraw` clears `Dial::sent` when the layout changes.
- Settings come in two kinds. Plugin-wide ones (`src/config.rs`) are OpenDeck *global settings*: the plugin asks for them in `plugin_ready` and gets them back in `did_receive_global_settings` whenever the property inspector saves them. Per-dial ones are `action::Settings`, delivered by `did_receive_settings`. **Defaults are duplicated** in `Config::default`, `action::Settings` and `GLOBAL_DEFAULTS` in `assets/pi/index.html`; keep them in sync. Read settings with `config::get()`, which returns a copy (no lock across `.await`).
- `assets/pi/index.html`: the property inspector (plain HTML/JS, no build step). OpenDeck serves it over HTTP, so it fetches its strings from `../lang/<lang>.json` (`PropertyInspector` section). It asks the plugin for the running apps with `sendToPlugin {cmd: "getApps"}`; the plugin answers with an `appList` message (`action::send_inspector_state`).
- `src/names.rs`: `assets/app-names.json` (installed next to the binary, hot-reloaded on mtime change, embedded copy as fallback) maps reported names to display names or `@system`. Backends call `names::lookup` with candidates in priority order (Linux: portal app id, binary, application.name, stream name). `@system` also changes the grouping key to `audio::SYSTEM_KEY`.
- `src/i18n.rs`: LCD strings from the `Localization` section of `assets/lang/{en,fr}.json`, embedded with `include_str!`; language from OpenDeck's `-info` argument, then `LANG`. A test checks both files have the same keys. OpenDeck itself only reads manifest translations from `<plugin>/<lang>.json`, so `scripts/install.sh` also copies them to the plugin root. LCD strings must stay short (the 22 px volume line fits ~11 characters).
- `src/icons/`: finds app icons (Linux: `.desktop` files and icon themes; Windows: extracted from the executable) and turns them into 56×56 PNG data URLs for the `app_icon` pixmap.
- `src/main.rs`: a refresh loop runs on backend change events or every 2 s. It also redraws everything every 30 s in case the host lost the feedback.

## Pitfalls

- Never call `PLUGIN.dials.get`/`get_mut` while a guard or iterator on `PLUGIN.dials` is alive (including inside a `for … in dials.iter()` loop): DashMap deadlocks on its shard lock and the whole plugin freezes silently, with nothing in the log. Copy what you need out of the guard first, and drop it before any `.await`. The same goes for the `std` locks in `state` and `config`.
- Debugging a frozen or misbehaving dial: turn on debug logs and read the plugin log (path above). Every dial event, mixer change and LCD update is logged.
- To check an LCD screen without the device, render it with the same crate OpenDeck uses: `streamdeck-strip-render` (git dependency of OpenDeck, see its `src-tauri/Cargo.toml` for the revision). Load `assets/layouts/<layout>.json` with `get_incremental_renderer`, call `set_feedback` with the payload from `render.rs`, and save `get_image()` as PNG. Do this for any new LCD text or layout change: overflowing text is cut with an ellipsis.

## OpenDeck specifics

- OpenDeck draws the encoder LCD with the `streamdeck-strip-render` crate (200×100 canvas, Stream Deck layout JSON). Pixmaps accept `data:image/png;base64,…`. Its fonts have no glyphs such as `●`, so keep LCD text to plain characters.
- The OpenDeck Flatpak (`me.amankhanna.opendeck`) has no PulseAudio socket until the user runs `flatpak override --user --socket=pulseaudio me.amankhanna.opendeck`. Without the cookie file, the auth cookie must still be 256 bytes (zeros) or PipeWire answers `Invalid`. Host icons are visible under `/run/host/share` and `/run/host/user-share` through `XDG_DATA_DIRS`.
- The Flatpak runtime's glibc can be older than the host's. `scripts/install.sh` therefore builds a static `*-linux-musl` binary and installs it under the `-linux-gnu` file name that `assets/manifest.json` expects.
- Binary names must match `CodePaths` in `assets/manifest.json` and the `out:` names in `.github/workflows/release.yml` (Linux: musl builds named `appvolume-<arch>-unknown-linux-gnu`; Windows: `appvolume-x86_64-pc-windows-msvc.exe`; macOS: one universal `appvolume-macos`).
- Releases: bump `Version` in `assets/manifest.json` and `version` in `Cargo.toml`, push a `vX.Y.Z` tag and create the GitHub release; the workflow attaches `AppVolume-X.Y.Z.zip` to it. The tag must match the manifest version.
- The action UUID `com.mrbretzel.appvolume.dial` appears in both `src/action.rs` and `assets/manifest.json`.
