# x264video4osu

[![en](https://img.shields.io/badge/lang-en-blue.svg)](README.md) [![zh](https://img.shields.io/badge/lang-zh-red.svg)](README.zh-CN.md)

A tool for encoding osu! background videos.

![License](https://img.shields.io/badge/license-MIT-blue.svg)
![Platform](https://img.shields.io/badge/platform-Windows-lightgrey.svg)
![Framework](https://img.shields.io/badge/UI-Slint%20%2B%20Rust-orange.svg)

## Motivation

The Maruko Tools Box (小丸工具箱) is no longer updated and doesn't support H.265 or AV1. I needed a one-click tool for compressing osu videos, so I wrote this.

This is just an FFmpeg frontend to solve my personal video compression needs. Everything here can be done with command-line FFmpeg. Need more features? There are many open-source FFmpeg frontends available.

**Note**: This project is entirely vibe coded. Quality is not guaranteed. For entertainment purposes only.

## Features

- Supports **2-Pass VBR** and **CRF** encoding modes
- Real-time encoding progress display (percentage, elapsed time, ETA)
- Custom output resolution (scaling supported)
- Optional audio extraction
- Supports Chinese and English interface
- Detailed encoding log recording and export
- Drag-and-drop file input support

## Download & Setup

### Requirements

The following files are required in the `tools` folder (next to the executable):

- `ffmpeg.exe`
- `ffprobe.exe`

**Note**: The release version already includes these files. You only need to download FFmpeg separately if you're building from source.

Download from [gyan.dev](https://www.gyan.dev/ffmpeg/builds/) (GPL builds with x264 included).

### Usage

1. Place `ffmpeg.exe` and `ffprobe.exe` in the `tools` folder (next to `x264video4osu.exe`)
2. Run `x264video4osu.exe`
3. Select input video file
4. Configure encoding parameters (or use defaults)
5. Click "Start" to encode

> The release exe is a single self-contained binary (static CRT) — no VC++ runtime or .NET install required. Just copy the exe + `tools/` folder to any Windows 10/11 machine.

## Encoding Modes

The program offers two main encoding modes. **CRF is the recommended default** for osu! background videos (typically anime OPs): it's fast, and animation content compresses so well that a moderate CRF already looks great at low bitrate. Use **2-Pass** when you must hit a precise file size without iterating.

### CRF Mode (Constant Rate Factor) — Recommended Default

**How it works**: Constant quality factor encoding, prioritizing quality over file size.

| Item | Description |
|------|-------------|
| **Control method** | CRF value (range 0-51) |
| **Default value** | 26 |
| **Value meaning** | Lower value = better quality, larger file; higher value = lower quality, smaller file |
| **Output size** | Unpredictable (depends on video content complexity) |
| **Encoding speed** | Fast (single pass) |
| **Use case** | Most osu! background videos — especially anime OPs at low bitrate |

**CRF value reference**:

| CRF value | Quality | Description |
|-----------|---------|-------------|
| 18-23 | Higher | Near-lossless, larger file |
| 23-26 | Mid-high | Suitable for general purposes |
| 26-28 | Medium | General use, controllable file size |
| 28-32 | Lower | For low-bandwidth scenarios |
| 32+ | Low | Not recommended for background videos |

**Advantages**:
- ✅ Fast encoding speed (single pass)
- ✅ Simple configuration (only one quality value)
- ✅ **Anime OPs at low bitrate**: animation has clean edges and large flat-color regions, so a moderate CRF (26-28) relatively easily yields a satisfying result at a small file size

**Disadvantages**:
- ❌ File size is unpredictable — expect to encode a few times, adjusting the CRF value, until you land in the desired size range
- ❌ Complex or noisy scenes can exceed the osu! file size limit

---

### 2-Pass VBR Mode (for Strict File-Size Control)

**How it works**: First pass scans the video to analyze complexity, second pass allocates bitrate based on the analysis.

| Item | Description |
|------|-------------|
| **Control method** | Fixed target bitrate (kbps) |
| **Default value** | 800 kbps |
| **Output size** | Precisely controlled (bitrate × duration ≈ file size) |
| **Encoding speed** | Slower (requires two passes) |
| **Use case** | Set-and-forget: a strict size budget, no desire to iterate on CRF |

**Advantages**:
- ✅ Predictable and precisely controlled file size — set the bitrate, click start, walk away
- ✅ Allocates more bits to complex scenes, saves bits on simple scenes

**Disadvantages**:
- ❌ Encoding time is approximately 2x single-pass
- ❌ Relatively complex configuration (need to estimate appropriate bitrate)

**File size estimation example** (at 800 kbps):

| Duration | Estimated file size |
|----------|---------------------|
| 1 minute | ~6 MB |
| 1.5 minutes | ~9 MB |
| 2 minutes | ~12 MB |
| 2.5 minutes | ~15 MB |
| 3 minutes | ~18 MB |

---

## Recommended Configuration (osu! Background Videos)

For typical osu! background videos (usually anime OPs, under 2 minutes, < 15MB requirement):

| Parameter | Recommended value | Description |
|-----------|-------------------|-------------|
| Encoding mode | **CRF** (default) | Animation compresses well; iterate the CRF value to hit your size |
| CRF | **26-28** | Start at 26, then adjust by 1-2 to tune the output size |
| Resolution | **720p or original** | Keep vertical aspect ratio |
| FPS | **24** | Frame rate setting |

**Quick workflow**: encode once at CRF 26. If the file is too large, raise CRF by 1-2 and re-encode; if you have room to spare, lower it for better quality. Because anime OPs compress so efficiently, you'll usually settle on a satisfying size after one or two tries.

**Prefer set-and-forget?** If you must fit a fixed budget exactly — e.g. a beatmap with a strict file-size limit — skip the iteration and use **2-Pass** with an estimated bitrate. It costs roughly twice the encode time, but the output size lands predictably on the first try.

**Bitrate selection guide** (for 2-Pass):

- Video duration under 1 minute: `800-1000 kbps`
- Video duration 1-1.5 minutes: `700-900 kbps`
- Video duration 1.5-2 minutes: `600-800 kbps`

## Usage

### Basic Workflow

1. **Select input video**
   - Click "Browse" button to select file
   - Or drag and drop video file onto the window

2. **Set output path** (optional)
   - If not set, automatically generates `{original_name}_output.mp4` in the same directory

3. **Configure encoding parameters**

   | Parameter | Description |
   |-----------|-------------|
   | **Encoding Standard** | Select 2pass or CRF mode |
   | **Value box** | 2pass = bitrate, CRF = quality value |
   | **Resolution** | Width × Height, set to 0 to keep original |
   | **Scale Up** | Allow upscaling (disabled by default) |
   | **Extract Audio** | Extract audio track separately as .m4a file |

4. **Click "Start"** to begin encoding

5. **View progress**
   - Main interface shows real-time progress
   - "Log" tab shows detailed logs

6. **Completion**
   - Displays output file size
   - Click "Open Output Folder" to open output directory

### Log Function

- Encoding process displayed in real-time on the Log tab
- Save complete log via "Save Log" button
- Log file naming format: `log_yyyyMMddHHmmss_randomNumber.txt`

## Technical Parameters

### Default Encoding Parameters

The program uses the following fixed parameters:

| Parameter | Value | Description |
|-----------|-------|-------------|
| Encoder | libx264 | H.264 encoder |
| Preset | veryslow | Slower encoding |
| Profile | high | High profile |
| Level | 5.2 | High level configuration |
| Pixel format | yuv420p | Widely compatible pixel format |
| GOP size | 300 | Maximum keyframe interval |
| Minimum GOP | 240 | Minimum keyframe interval |
| B frames | 16 | High B-frame count |
| Motion estimation | UMH | UMH motion estimation |
| Subpixel subdivision | 11 | Level 11 |

### Advanced x264 Parameters

```
scenecut=0, ref=16, bframes=16, b-adapt=2, direct=auto,
me=umh, subme=11, trellis=2, rc-lookahead=60, aq-mode=3,
aq-strength=1.0, psy-rd=1.0,0.15, deblock=-1,-1,
weightp=2, cabac=1, merange=32
```

## Development

### Tech Stack

- Rust (edition 2021)
- Slint 1.17.1 (UI framework, Fluent style) + winit backend
- femtovg renderer (OpenGL; vendored local patch for correct DPI anti-aliasing)
- FFmpeg (via command-line invocation)

### Project Structure

```
x264video4osu/
├── src/                 # Rust application layer
│   ├── main.rs          # Entry point; validates FFmpeg tools, runs event loop
│   ├── app.rs           # AppController: wires UI callbacks ↔ services
│   ├── i18n.rs          # All UI strings (zh-CN / en-US)
│   ├── services/        # FFmpeg args, encoding orchestration, tool discovery
│   ├── platform/        # Drag-drop, timestamps
│   └── io/              # Opening URLs / folders
├── ui/                  # Slint UI definitions (.slint, compiled at build time)
│   ├── main.slint       # Main window
│   └── dialogs/         # about / ffmpeg_not_found / message dialogs
├── third_party/         # Vendored i-slint-renderer-femtovg (DPI patch)
├── build.rs             # slint_build compilation
├── tools/               # FFmpeg tools directory
└── release/             # Built release bundle (exe + tools/)
```

### Build

```bash
cargo build --release
cargo test
```

Requires a Rust toolchain with the `x86_64-pc-windows-msvc` target. The release build statically links the CRT (see `.cargo/config.toml`), producing a self-contained exe with no runtime dependencies.

## License

This project is released under the MIT License.

The included FFmpeg programs (ffmpeg.exe, ffprobe.exe) are from [gyan.dev](https://www.gyan.dev/ffmpeg/builds/), released under the **GPL v3** license.
