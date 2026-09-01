# x264video4osu — AI Coding Agent Instructions

## Project Overview

**x264video4osu** is a Windows desktop application (Rust + Slint 1.17.1) for compressing osu! background videos with FFmpeg/x264. It is the port of the original C#/WPF app and is functionally equivalent (2-Pass VBR / CRF, resolution scaling, audio extraction, real-time progress, bilingual UI).

## Architecture & Key Components

### UI layer — Slint (`.slint`, in `ui/`)
- `ui/main.slint` — main window: tabs (video/log), input/output path, format ComboBox, CRF/2pass RadioGroup, resolution LineEdits, CheckBoxes, progress bar, start/stop buttons.
- `ui/dialogs/` — `about.slint`, `ffmpeg_not_found.slint`, `message.slint` (compiled to Rust types by `build.rs`).
- Slint files are compiled at **build time** via `slint_build` (see `build.rs`) — after editing `.slint` you MUST rebuild, otherwise the app runs a stale binary.
- Fluent style; rendered by the **femtovg** renderer (winit backend).

### Rust application layer — `src/`
- `main.rs` — entry point; validates FFmpeg tools first, runs the event loop. Includes the generated `.slint` types.
- `app.rs` — `AppController`: owns the UI handle, wires callbacks ↔ services, drives the log list and progress updates. Contains a 100ms `Timer` workaround (see below).
- `i18n.rs` — all UI strings per language (zh-CN / en-US); applied in bulk by `UiStrings`.
- `error.rs` — `AppError` enum (`AppResult = Result<T, AppError>`).
- `services/` — FFmpeg argument building (`args.rs`), encoding orchestration (`ffmpeg.rs`), tool discovery (`ffmpeg_config.rs`), scale filter (`scale.rs`), path utilities (`pathutil.rs`).
- `platform/` — drag-drop, timestamps (via `windows-sys` `GetLocalTime`).
- `io/` — opening URLs / folders.

## Critical Developer Workflows

### Build & Run
```bash
cargo build          # dev
cargo build --release
cargo test
```

### Runtime requirements
- FFmpeg tools `ffmpeg.exe` + `ffprobe.exe` must exist in a `tools/` folder **next to the executable** (or relative to cwd). The app shows a "download / exit" dialog when missing.
- Release is built with static CRT (`.cargo/config.toml` → `+crt-static`), so the exe has no VC++ runtime dependency.

### Known gotchas (VERY IMPORTANT — do not regress these)
- **femtovg renderer patch**: `third_party/i-slint-renderer-femtovg` is a vendored copy with a local patch — device pixel ratio is fixed at `1.0` because Slint draws in physical pixels; the upstream `ceil(scale_factor)` logic produced 0.5px AA fringes (jagged radio buttons/checkboxes at 125% DPI). Do NOT delete the `[patch.crates-io]` entry in `Cargo.toml` or revert that file.
- **Log view**: `LogView` in `ui/main.slint` uses `ListView` (one row per line). Do NOT switch it to "ScrollView + single Text" — the width binding creates a binding loop that never overflows, breaking scrolling. Auto-scroll sets `viewport-y` to a NEGATIVE value (scroll offset is `-viewport-y`).
- **Slint expression limits**: no `100% - 2px` style arithmetic; use `parent.width - 2px`.
- **app.rs 100ms Timer**: a periodic timer exists as a workaround for a Slint window-size/DPI issue — don't remove it casually.
- **Layout gotchas**: layout children have no `margin` property (use `padding-left` on a wrapping `HorizontalBox`); fluent `RadioGroup` default stretching spreads buttons apart.
- **Tooltip/format hint**: a click-opens popup (`FormatTip`), not hover — hover-only had a `has-hover` flicker issue.

## Code Conventions

- **Comments**: Chinese, explaining *why* (they document workarounds and the correspondence to the old C# program). Keep them.
- Errors propagate as `AppError`; services return `AppResult`.
- Tests live in `#[cfg(test)]` modules next to code (e.g. `src/services/args.rs`, `src/log_layout_test.rs`). `cargo test` uses `i-slint-backend-testing`; real-ffmpeg end-to-end tests exist in `services::ffmpeg`.

## Testing
- `cargo test` runs unit tests + headless UI layout tests + (when ffmpeg present) real encodes. All must pass.
