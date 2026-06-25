# x264video4osu - AI Coding Agent Instructions

## Project Overview
**x264video4osu** is a WPF desktop application for converting video files to MP4 format with x264 codec optimization, designed for the rhythm game osu!. Built with .NET 8, C#, and XAML.

## Architecture & Key Components

### 1. **WPF Application Structure** (Model-View Pattern)
- **MainWindow.xaml**: UI layer - controls, input fields, logging display
- **MainWindow.xaml.cs**: Code-behind - handles user interactions and business logic coordination
- **Services/**: Business logic and utility classes

**Data Flow**: User Input (XAML) → Event Handler (Code-behind) → BuildFfmpegArgs() → FfmpegService → FFmpeg Process

### 2. **Core Services**

#### `FfmpegService` (Services/FfmpegService.cs)
- Manages FFmpeg subprocess execution
- **Start(args, log callback)**: Launches FFmpeg with specified arguments, streams stderr to UI via callback
- **Stop()**: Terminates FFmpeg process tree with `Kill(true)`
- Uses **asynchronous process monitoring** - error output is read line-by-line and invoked on UI thread
- **Key pattern**: Pass `Action<string>` callback for logging to decouple service from UI

#### `ScaleHelper` (Services/ScaleHelper.cs)
- Static utility for FFmpeg scaling filter generation
- Handles three scale modes:
  - **Fixed dimensions**: `scale=w:h`
  - **Proportional (no upscale)**: Conditional filters `if(gt(iw,w),w,iw)` to prevent upscaling
  - **Proportional (upscale allowed)**: Direct `-1` for aspect preservation
- Used in `BuildFfmpegArgs()` with width/height/upscale checkbox inputs

### 3. **Video Encoding Pipeline**

FFmpeg arguments are built with **two-pass encoding** (internally stored):
- **Pass 1**: Analysis pass → `NUL` (Windows null device)
- **Pass 2**: Final encode → output MP4 with `+faststart` flag
- **x264 Parameters**: Highly optimized for osu! beatmap videos:
  - `preset=veryslow` (quality over speed)
  - `profile=high`, `level=5.2` (compatibility)
  - `ref=16`, `bframes=16`, `me=umh` (quality settings)
  - `rc-lookahead=60`, `aq-mode=3`, `psy-rd=1.0,0.15` (perceptual optimization)

## Critical Developer Workflows

### Build & Run
```bash
dotnet build
dotnet run  # Or launch from Visual Studio
```

### Dependencies
- **.NET 8 SDK** with WPF support (`UseWPF=true` in .csproj)
- **FFmpeg** executable (must be in PATH)
- No NuGet packages required (uses only .NET stdlib)

### Debugging Tips
- **Process output**: Set breakpoint in `FfmpegService.Start()` - stderr output is received asynchronously
- **UI thread safety**: All UI updates use `Dispatcher.Invoke()` - required because FFmpeg callback runs on different thread
- **Drag-drop input**: Handled by `Input_Drop()` event attached to Window `AllowDrop=true`

## Code Conventions & Patterns

### Naming
- **Private fields**: `_camelCase` prefix (e.g., `_ffmpeg`, `_log`)
- **XAML element names**: `PascalCaseBox`/`PascalCaseCheck` suffixes (e.g., `InputPathBox`, `ScaleUpCheck`)
- **Methods**: PascalCase, event handlers append `_Click` or `_Changed`

### C# Features
- **File-scoped namespaces**: `namespace x264video4osu;` (modern C# style)
- **Nullable reference types**: Enabled (`<Nullable>enable</Nullable>`)
- **Implicit usings**: Enabled
- **Readonly fields**: `private readonly FfmpegService _ffmpeg = new();`
- **String interpolation**: `$"{variable}"` with escaping for paths `\"{path}\"`

### XAML Patterns
- **Resource bindings**: `{DynamicResource KeyName}` for i18n strings (see Resources/Strings.*.xaml)
- **Layout**: Grid with explicit row/column definitions + Border containers for sections
- **Height consistency**: Most interactive elements use `Height="36"` or `"44"`
- **Margins**: `Margin="0,0,12,8"` for spacing (right, bottom emphasis)

### Localization
- **Two languages**: Chinese (zh-CN) and English (en-US)
- **Resource files**: XAML-based at `Resources/Strings.{lang}.xaml`
- **Runtime switching**: `LanguageChanged()` clears and reloads resource dictionaries
- **All UI strings**: Stored as `{DynamicResource}` bindings, not hardcoded

## Integration Points & External Dependencies

### FFmpeg Integration
- **Command structure**: `-i input.mp4 -vf "FILTER" -c:v libx264 [OPTIONS] output.mp4`
- **Key flags**:
  - `-pass 1/2`: Two-pass encoding
  - `-vf`: Video filter (scale, etc.)
  - `-x264-params`: Raw x264 codec options
  - `-movflags +faststart`: Enable YouTube-style streaming
  - `-an`: Remove audio (unless ExtractAudioCheck implemented)
- **Process interaction**: Windows null device `NUL` for pass 1 output

### Logging & User Feedback
- **Log accumulation**: `StringBuilder _log` persists entire session
- **Real-time UI update**: `LogBox.Text = _log.ToString()` on each line + `ScrollToEnd()`
- **Persistence**: `SaveLog_Click()` writes to `log.txt`

## Common Modification Points

| Task | Files | Notes |
|------|-------|-------|
| Add encoding options | MainWindow.xaml, BuildFfmpegArgs() | Add TextBox, parse value, append to args string |
| Change video codec | FfmpegService, BuildFfmpegArgs() | Replace `-c:v libx264` and x264-params |
| Modify UI layout | MainWindow.xaml | Adjust Grid rows/columns or Border padding |
| Add language | Resources/Strings.{lang}.xaml | Create new resource file, add to LanguageBox combobox |
| Extract audio feature | MainWindow.xaml.cs, BuildFfmpegArgs() | Conditionally add `-c:a aac` based on checkbox |

## Testing Strategy
- **Manual integration testing**: Drag video file → adjust params → click Start → verify output
- **FFmpeg validation**: Check log for encoding errors (lines starting with "error" or "Error")
- **Process termination**: Click Stop button should cleanly kill FFmpeg process

## Notes for AI Agents
- **Thread safety critical**: FFmpeg async output requires `Dispatcher.Invoke()` for any UI changes
- **Error handling**: Currently minimal - stderr from FFmpeg is logged but not parsed for errors
- **Path handling**: Always quote paths with `\"{path}\"` when building FFmpeg args (spaces in filenames)
- **Two-pass encoding**: `_ffmpeg.Pass1Args` is stored internally for future pass 1 execution (currently pass 1 outputs to NUL)
