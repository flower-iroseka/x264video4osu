# x264video4osu Wiki

[中文](Home.zh-CN.md) · **English**

[← Back to README](../README.md)

This wiki supplements the README with material it doesn't cover: how the two encoding modes differ, recommended settings for osu! background videos, and the x264 parameters the program uses.

## Encoding Modes

The program offers two encoding modes, **CRF** and **2-Pass VBR**, with CRF recommended by default. osu! background videos are usually anime OPs, and such footage tends to have sharp edges and large areas of flat color, which compress easily. A single CRF pass usually balances quality, file size, and encoding speed well. When the file size needs to be more predictable, use 2-Pass VBR.

### CRF Mode (Constant Rate Factor)

**How it works:** The CRF value controls the encoding quality; the actual bitrate follows the complexity of the footage.

| Item | Description |
|------|-------------|
| Control method | Set a CRF value from 1 to 51 |
| Default value | 26 |
| Value meaning | A lower CRF value usually means better quality and a larger file; a higher value means lower quality and a smaller file |
| Output size | Cannot be predicted from the CRF value alone; the actual size depends on the video content |
| Encoding speed | Single pass, usually faster than 2-Pass VBR |
| Use case | Most osu! background videos, especially anime OPs where a small file is wanted quickly |

**CRF value reference:**

| CRF value | Quality reference | Description |
|-----------|-------------------|-------------|
| 18–23 | Higher | Quality is usually good, but the file may be large |
| 23–26 | Mid-high | Can serve as a reference range for general purposes |
| 26–28 | Medium | Can serve as a starting point for test encodes of osu! background videos |
| 28–32 | Lower | The file is usually smaller, but the loss is more visible |
| 32 and up | Lower | Generally not recommended for osu! background videos |

**Advantages:**

- Only a CRF value has to be set; no target bitrate has to be estimated in advance.
- The sharp edges and large flat-color areas common in anime OPs usually compress easily, so CRF 26–28 can serve as a starting range that balances quality against file size; the actual result still depends on the content.

**Limitations:**

- The output file size cannot be set directly. Meeting a specific size limit may take several encodes with the CRF value adjusted in between.
- Complex footage or footage with heavy noise may produce a larger output file that exceeds the target size.

### 2-Pass VBR Mode (Two-Pass Variable Bitrate)

**How it works:** The first pass analyzes the video content; the second distributes bits from that analysis to reach the target bitrate.

| Item | Description |
|------|-------------|
| Control method | Set a target video bitrate in kbps |
| Default value | When the bitrate box is empty or the entered number isn't positive, the program uses 800 kbps |
| Output size | Easier to predict and control than CRF mode; the actual file size is also affected by audio data, container overhead, and other factors |
| Encoding speed | Two passes are required, so the total time is usually longer |
| Use case | Planning the file size around a target, or avoiding repeated CRF adjustments |

**Advantages:**

- The required bitrate can be estimated from the video duration and the target size, which reduces the number of test encodes.
- From the first pass, the encoder can give complex scenes more bits and simple scenes fewer.

**Limitations:**

- Two passes are required, so it usually takes longer than a single CRF pass.
- The bitrate has to be chosen for the video duration and the target file size; a poor choice can leave the final size off target.

**File size estimation example:** Estimated at a video bitrate of 800 kbps, excluding audio data and container overhead, so the actual output file may be larger.

| Video duration | Estimated video size |
|----------------|----------------------|
| 1 minute | ~6 MB |
| 1.5 minutes | ~9 MB |
| 2 minutes | ~12 MB |
| 2.5 minutes | ~15 MB |
| 3 minutes | ~18 MB |

## Recommended Configuration (osu! Background Videos)

For a typical osu! background video — an anime OP under 2 minutes long, with a target file size of about 15 MB or less — start from the configuration below:

| Parameter | Recommended value | Description |
|-----------|-------------------|-------------|
| Encoding mode | **CRF** (default) | Good for quick test encodes; the output size changes with the CRF value |
| CRF | **26–28** | Encode at 26 first, then adjust from the output size |
| Resolution | **720p, or the original size if it meets the limit** | Keep the source aspect ratio; osu! requires the output to be no larger than 1280 × 720 |
| FPS | **24** | Output is fixed at 24 fps; the interface has no control for it |

**Suggested workflow:** Encode once at CRF 26. If the output file is too large, raise the CRF value by 1–2 and encode again; if there is room to spare and higher quality is wanted, lower the CRF value by 1–2. Depending on how complex the footage is, more than one round may be needed.

If the file size has to stay closer to a specific target — for example a beatmap with a strict size limit — switch to **2-Pass VBR** and estimate the target bitrate from the video duration. Two passes usually take longer, but the output size is easier to predict.

**2-Pass VBR bitrate reference:**

- Video duration up to 1 minute: `800–1000 kbps`
- Video duration 1–1.5 minutes: `700–900 kbps`
- Video duration 1.5–2 minutes: `600–800 kbps`

These ranges are only a starting estimate. The bitrate actually needed varies with the complexity of the footage, the audio data, and the target file size.

## Technical Parameters

### Default Encoding Parameters

Fixed encoding parameters the program uses:

| Parameter | Value | Description |
|-----------|-------|-------------|
| Encoder | `libx264` | Encoder that produces the H.264 video |
| Encoding preset | `veryslow` | Takes longer to encode, and generally favors compression efficiency |
| Encoding profile | `high` | H.264 High Profile |
| Encoding level | `5.2` | H.264 Level 5.2 |
| Pixel format | `yuv420p` | A widely compatible pixel format |
| GOP size | `300` | Maximum keyframe interval |
| Minimum GOP | `240` | Minimum keyframe interval |
| B-frame count | `16` | Sets the upper limit on B-frames |
| Motion estimation algorithm | `UMH` | Uses the UMH motion estimation algorithm |
| Subpixel refinement level | `11` | Sets the refinement level for subpixel motion estimation |

### Advanced x264 Parameters

The program uses the following x264 parameter string:

```text
scenecut=0:ref=16:bframes=16:b-adapt=2:direct=auto:me=umh:subme=11:trellis=2:rc-lookahead=60:aq-mode=3:aq-strength=1.0:psy-rd=1.0,0.15:deblock=-1,-1:weightp=2:cabac=1:merange=32
```

This string is passed through FFmpeg's `-x264-params` option and defined in the `X264_PARAMS` constant in `src/services/args.rs`. Parameters are separated by colons; a comma inside a parameter value (as in `psy-rd=1.0,0.15` and `deblock=-1,-1`) belongs to the value itself, and is not a parameter separator.
