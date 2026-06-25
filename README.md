# x264video4osu

[![en](https://img.shields.io/badge/lang-en-blue.svg)](README.md) [![zh](https://img.shields.io/badge/lang-zh-red.svg)](README.zh-CN.md)

A tool for encoding osu! background videos.

![License](https://img.shields.io/badge/license-MIT-blue.svg)
![Platform](https://img.shields.io/badge/platform-Windows-lightgrey.svg)
![Framework](https://img.shields.io/badge/.NET-10.0-purple.svg)

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

The following files are required in the `tools` folder:

- `ffmpeg.exe`
- `ffprobe.exe`

Download from [FFmpeg official website](https://ffmpeg.org/download.html) or use builds from [gyan.dev](https://www.gyan.dev/ffmpeg/builds/).

### Usage

1. Place `ffmpeg.exe` and `ffprobe.exe` in the `tools` folder
2. Run `x264video4osu.exe`
3. Select input video file
4. Configure encoding parameters (or use defaults)
5. Click "Start" to encode

## Encoding Modes

The program offers two main encoding modes:

### 2-Pass VBR Mode (Recommended for osu! Backgrounds)

**How it works**: First pass scans the video to analyze complexity, second pass allocates bitrate based on the analysis.

| Item | Description |
|------|-------------|
| **Control method** | Fixed target bitrate (kbps) |
| **Default value** | 800 kbps |
| **Output size** | Precisely controlled (bitrate × duration ≈ file size) |
| **Encoding speed** | Slower (requires two passes) |
| **Use case** | Projects with strict file size requirements |

**Advantages**:
- ✅ Predictable and precisely controlled file size
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

### CRF Mode (Constant Rate Factor)

**How it works**: Constant quality factor encoding, prioritizing quality over file size.

| Item | Description |
|------|-------------|
| **Control method** | CRF value (range 0-51) |
| **Default value** | 26 |
| **Value meaning** | Lower value = better quality, larger file; higher value = lower quality, smaller file |
| **Output size** | Unpredictable (depends on video content complexity) |
| **Encoding speed** | Fast (single pass) |
| **Use case** | Quality-first projects with relaxed file size requirements |

**CRF value reference**:

| CRF value | Quality | Description |
|-----------|---------|-------------|
| 18-23 | Higher | Near-lossless, larger file |
| 23-26 | Mid-high | Suitable for general purposes |
| 26-28 | Medium | General use, controllable file size |
| 28-32 | Lower | For low-bandwidth scenarios |
| 32+ | Low | Not recommended for background videos |

**Advantages**:
- ✅ Fast encoding speed
- ✅ Simple configuration (only need to set one quality value)

**Disadvantages**:
- ❌ Cannot precisely control file size
- ❌ Complex scenes may exceed osu! file size limit

---

## Recommended Configuration (osu! Background Videos)

For typical osu! background videos (under 2 minutes, < 15MB requirement), the following configuration is recommended:

| Parameter | Recommended value | Description |
|-----------|-------------------|-------------|
| Encoding mode | **2-Pass** | Precise file size control |
| Bitrate | **800-1000 kbps** | Adjust based on video length |
| Resolution | **720p or original** | Keep vertical aspect ratio |
| FPS | **24** | Frame rate setting |

**Bitrate selection guide**:

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

- .NET 10.0 (WPF)
- C# 12+
- FFmpeg (via command-line invocation)

### Project Structure

```
x264video4osu/
├── Services/          # Core service layer
│   ├── FfmpegService.cs   # FFmpeg wrapper
│   └── FfmpegConfig.cs    # Configuration management
├── Resources/         # Resource files
│   ├── Strings.zh-CN.xaml  # Chinese resources
│   └── Strings.en-US.xaml  # English resources
├── tools/            # FFmpeg tools directory
├── MainWindow.xaml   # Main window
└── AboutDialog.xaml  # About dialog
```

### Build

```bash
dotnet build
```

## License

This project is released under the MIT License.

The included FFmpeg programs (ffmpeg.exe, ffprobe.exe) are from [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases), released under the **LGPL v3** license.