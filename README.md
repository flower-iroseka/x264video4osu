# x264video4osu

**English** · [中文](README.zh-CN.md)

A tool for encoding osu! background videos.

![License](https://img.shields.io/badge/license-MIT-blue.svg)
![Platform](https://img.shields.io/badge/platform-Windows-lightgrey.svg)
![Framework](https://img.shields.io/badge/UI-Slint%20%2B%20Rust-orange.svg)

## Motivation

Maruko Tools Box (小丸工具箱) is no longer maintained and no longer supports H.265 or AV1 encoding. I developed this tool to make encoding osu! background videos more convenient.

This project is an FFmpeg frontend intended primarily for my own video-encoding needs. The same tasks can be performed using FFmpeg from the command line. For additional video-encoding features, other open-source FFmpeg frontends are available.

**Note:** This project was developed entirely through vibe coding. Quality is not guaranteed; the project is provided for entertainment purposes only.

## Features

- Supports **2-Pass VBR** and **CRF** encoding modes.
- Displays real-time encoding progress, including completion percentage, elapsed time, and estimated time remaining.
- Supports custom output resolutions and scaling.
- Provides three audio modes: no audio, copy the audio stream, or transcode audio to AAC or FLAC.
- Provides video preview with playback, frame-by-frame navigation, and seeking. The current position can be used as the trim start or end.
- Supports trimming by frame number or time; see [Trimming Video](#trimming-video).
- Supports exporting the audio track separately, keeping the source codec; see [Extracting Audio](#extracting-audio).
- Supports Chinese and English interfaces.
- Records and exports detailed encoding logs.
- Supports drag-and-drop video input.

## Download and Setup

### Requirements

Place the following files in the `tools` directory, alongside `x264video4osu.exe`:

- `ffmpeg.exe`: Download a GPL build that includes x264 from [gyan.dev](https://www.gyan.dev/ffmpeg/builds/).
- `libmpv-2.dll` (optional; required only for video preview): Download `mpv-dev-lgpl-x86_64-*.7z` from [zhongfly's mpv Windows builds](https://github.com/zhongfly/mpv-winbuild/releases). The LGPL build keeps it compatible with this project's MIT license.

**Note:** Release builds already include both files. These files only need to be prepared manually when building from source.

Once the files are in place, run `x264video4osu.exe`. For the workflow, see [Usage](#usage).

## Encoding Modes and Recommended Settings

**CRF** (constant quality) is the default mode and is usually simpler to use. If a beatmap has a strict video file-size limit, use **2-Pass VBR** for more precise control over the output size.

The wiki covers how both encoding modes work, CRF value references, file-size estimates, recommended settings for osu! background videos, and the x264 parameters used by the program: [Encoding Modes, Recommended Settings, and Technical Parameters](wiki/Home.md).

## Usage

### Basic Workflow

1. **Select the input video.**
   - Click **Browse** to select a video file.
   - Alternatively, drag and drop the video file onto the program window.

2. **Select the output format.**
   - `flv` or `avi` is recommended. osu! stable has a known issue in which MP4 background videos may cause the game to crash randomly. See the [related discussion](https://osu.ppy.sh/community/forums/topics/1976015?n=1).

3. **Set the output path (optional).**
   - If no output path is specified, the program saves the file in the input file's directory as `{original_name}_output.flv`. The extension follows the selected output format.

4. **Configure the encoding parameters.**

   | Parameter | Description |
   | --- | --- |
   | **Output format** | Supported formats: `flv`, `avi`, and `mp4`. |
   | **Encoding Standard** | Select `2-Pass VBR` or `CRF`. |
   | **Value field** | In 2-Pass VBR mode, specifies the bitrate; in CRF mode, specifies the quality value. |
   | **Resolution** | Set the output width and height. Set to `0` to preserve the original value. |
   | **Scale Up** | Allows upscaling; disabled by default. |
   | **Audio mode** | No audio (default), copy the audio stream, or transcode to AAC / FLAC. FLAC is available only for MP4. |
   | **Trim video** | Enable trimming by frame number or time; see [Trimming Video](#trimming-video). |
   | **Extract Audio** | Export the audio track as a separate file; see [Extracting Audio](#extracting-audio). |

5. **Start encoding.**
   - Click **Start**.

6. **Monitor progress.**
   - The main interface displays real-time encoding progress.
   - The **Log** tab displays detailed logs.

7. **Check the result.**
   - When encoding finishes, the program displays the output file size.
   - Click **Open Output Folder** to open the output directory.

### Trimming Video

When **Trim video** is enabled, the program trims the chosen segment and encodes it.

#### Setting the Trim Range

| Control | Description |
| --- | --- |
| **Trim unit** | Select **Frames** or **Time**. Time accepts the `HH:MM:SS.mmm` format, such as `0:01:23.500`; `MM:SS` and seconds alone are also accepted. |
| **Start / End** | Enter a frame number or timestamp according to the selected unit. |

When the trim unit changes, the program automatically converts the entered start and end values to the new unit. No manual conversion is needed. The conversion requires the source video's frame rate, which the program reads automatically.

#### Using the Preview to Set the Range

The narrow strip along the right edge of the program window opens the video preview panel. In the panel, drag the seek bar, or enter a position in the frame-number or timestamp field and click **Jump**.

To set the trim range:

- Click **Set start** to use the current position as the trim start.
- Click **Set end** to use the current position as the trim end.

#### Keyboard Shortcuts

The following shortcuts work only when focus is not in an input field:

| Shortcut | Action |
| --- | --- |
| `Space` | Play / pause |
| `D` / `F` | Step frame by frame |
| `←` / `→` | Seek backward / forward by 5 seconds |
| `Ctrl` + arrow key | Seek backward / forward by 30 seconds |
| `Shift` + arrow key | Seek backward / forward by 1 minute |
| `Home` / `End` | Jump to the beginning / end of the video |
| `[` / `]` | Set the trim start / end to the current position |
| `↑` / `↓` | Increase / decrease volume |
| `M` | Mute / unmute |

#### Notes

- Trimming by frame number requires the source video's frame rate to be readable. If the frame rate cannot be read, the program suggests trimming by time instead. This can occur with some variable-frame-rate videos.

### Extracting Audio

Click **Extract Audio** to export the source audio track as its own file, at the same quality as the source.

The audio goes to the same place the video output would, named `name_audio.ogg`.

If **Trim video** is enabled, the exported audio is limited to the same trim range as the video.

### Logging

- Click **Save Log** to save the complete log.
- Log file names use the format `log_yyyyMMddHHmmss_randomNumber.txt`.

### Language Switching

Use the **Language** dropdown in the bottom-right corner of the window to switch between 中文 and English. The change takes effect immediately.

## Development

### Tech Stack

- Rust (edition 2021)
- Slint 1.17.1: UI framework with Fluent styling and the winit backend
- femtovg: OpenGL renderer; a local patch corrects anti-aliasing at different DPI scales
- FFmpeg: invoked through the command line

### Build

```bash
cargo build --release
cargo test
```

Building requires a Rust toolchain with the `x86_64-pc-windows-msvc` target. The Release build statically links the CRT through `.cargo/config.toml`, producing a self-contained executable that does not depend on an external CRT runtime.

## License

This project is released under the MIT License.

The included FFmpeg executable (`ffmpeg.exe`) is from [gyan.dev](https://www.gyan.dev/ffmpeg/builds/) and is released under the **GPL v3** license.

The included mpv library (`libmpv-2.dll`) is an LGPL build of [mpv](https://mpv.io/) from [zhongfly/mpv-winbuild](https://github.com/zhongfly/mpv-winbuild), and is released under the **LGPL v2.1 or later** (LGPLv2.1+) license.