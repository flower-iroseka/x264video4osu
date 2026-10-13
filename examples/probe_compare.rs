//! 对照程序：把 ffmpeg 单独解析出来的元数据和 ffprobe 的并排打出来。
//!
//! 这是"去掉 ffprobe.exe"这件事的验收工具。跑的就是主逻辑将来要用的那份代码——
//! 它 include 的是 `src/services/probe.rs` 本尊（走 `#[path]`，因为本 crate 没有 lib
//! target，example 够不到 `crate::services`），不存在"对照程序自己另写一份解析"的问题。
//!
//! 它同时也会动手建帧表（主逻辑只在可疑 VFR 时才建），因为帧表正是要验收的东西。
//!
//! 跑法（不带参数测内置的那几个，不存在的自动跳过）：
//!   cargo run --example probe_compare
//!
//! 测你自己的视频：
//!   cargo run --example probe_compare -- "D:\某个视频.mkv"
//!
//! 想一次测多个就多给几个路径。`tools/ffprobe.exe` 不在时只打新侧，不报错。

#[path = "../src/services/probe.rs"]
mod probe;

use probe::{Banner, FrameTable, Rational};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 不带参数时测这几个，不存在的跳过
const DEFAULT_FILES: &[&str] = &[
    r"tests-001\demo_43.mp4",
    r"tests-001\fps23976.mp4",
    r"tests-001\tb_test.mkv",
    r"tests-001\tb_test.flv",
    r"tests-001\tb_test.mov",
    r"tests-001\avi_real.avi",
    r"tests-001\vfr.mp4",
    r"tests\【圆神】青空之泪-[N.ec鱼丸].mp4",
];

// ---- ffprobe 那一侧：参数逐字照抄现在 ffmpeg.rs 里的写法（去掉 ffprobe 之后
//      这段连同旧函数一起删）----

fn run(tool: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn parse_rational(s: &str) -> f64 {
    match s.trim().split_once('/') {
        Some((n, d)) => {
            let n: f64 = n.trim().parse().unwrap_or(0.0);
            let d: f64 = d.trim().parse().unwrap_or(0.0);
            if d == 0.0 {
                0.0
            } else {
                n / d
            }
        }
        None => s.trim().parse().unwrap_or(0.0),
    }
}

/// 现在 `probe_media` 拿的东西
#[derive(Default, Clone, Copy)]
struct OldMedia {
    duration: f64,
    avg_fps: f64,
    raw_fps: f64,
    nb_frames: i64,
    width: u32,
    height: u32,
}

fn old_media(ffprobe: &Path, file: &str) -> Option<OldMedia> {
    let text = run(
        ffprobe,
        &[
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "stream=avg_frame_rate,r_frame_rate,nb_frames,width,height",
            "-show_entries", "format=duration",
            "-of", "default=noprint_wrappers=1",
            file,
        ],
    )?;

    let mut m = OldMedia::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "avg_frame_rate" => m.avg_fps = parse_rational(v),
            "r_frame_rate" => m.raw_fps = parse_rational(v),
            "nb_frames" => m.nb_frames = v.trim().parse().unwrap_or(0),
            "duration" => m.duration = v.trim().parse().unwrap_or(0.0),
            "width" => m.width = v.trim().parse().unwrap_or(0),
            "height" => m.height = v.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    Some(m)
}

fn old_audio(ffprobe: &Path, file: &str) -> Option<String> {
    let text = run(
        ffprobe,
        &[
            "-v", "error",
            "-select_streams", "a:0",
            "-show_entries", "stream=codec_name",
            "-of", "default=noprint_wrappers=1:nokey=1",
            file,
        ],
    )?;
    let s = text.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// `probe_repeated_timeline` 现在拿的东西
fn old_packets(ffprobe: &Path, file: &str) -> i64 {
    let Some(text) = run(
        ffprobe,
        &[
            "-v", "error",
            "-select_streams", "v:0",
            "-count_packets",
            "-show_entries", "stream=nb_read_packets",
            "-of", "default=noprint_wrappers=1",
            file,
        ],
    ) else {
        return 0;
    };
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "nb_read_packets" {
                return v.trim().parse().unwrap_or(0);
            }
        }
    }
    0
}

// ---- 打印 ----

const W: usize = 24;

fn row(label: &str, old: String, new: String, ok: bool) {
    println!(
        "  {:<12} {:<W$} {:<W$} {}",
        label,
        old,
        new,
        if ok { "一致" } else { "★ 不一致" },
        W = W
    );
}

/// 只打新侧（ffprobe 不在，或者这一项 ffprobe 本来就没有）
fn row_new(label: &str, new: String) {
    println!("  {:<12} {:<W$} {:<W$}", label, "-", new, W = W);
}

fn fmt_fps(v: f64) -> String {
    if v <= 0.0 {
        "-".into()
    } else {
        format!("{v:.6}")
    }
}

fn fmt_rat(r: Rational) -> String {
    if !r.is_valid() {
        return "-".into();
    }
    if r.den == 1 {
        format!("{}", r.num)
    } else {
        format!("{}/{}", r.num, r.den)
    }
}

fn show(ffmpeg: &Path, ffprobe: Option<&Path>, file: &str) {
    println!("\n{}", "=".repeat(96));
    println!("{file}");
    println!("{}", "=".repeat(96));

    let banner: Banner = probe::read_banner(ffmpeg, file);
    if !banner.opened {
        println!("  ffmpeg 打不开这个文件（横幅里没有 Input #0）");
        return;
    }

    let exact = probe::exact_frame_rate(ffmpeg, file);
    let suspects = probe::banner_suspects_vfr(&banner);

    // 帧表：主逻辑只在可疑 VFR 时才建，对照程序一律建一遍看结果
    let mut packets_seen = 0usize;
    let table: Option<FrameTable> = probe::build_frame_table(
        ffmpeg,
        file,
        &AtomicBool::new(false),
        &mut |n| packets_seen = n,
    );
    let _ = packets_seen;

    // 二级判据。主逻辑会拿它决定要不要留着这张表。
    let verdict = match (&table, exact) {
        (Some(t), Some(r)) => Some(probe::is_vfr(t.len(), t.span_secs(), r)),
        _ => None,
    };
    let vfr = verdict == Some(true);

    println!(
        "  {:<12} {:<W$} {:<W$} {}",
        "字段",
        "ffprobe",
        "ffmpeg（新）",
        "",
        W = W
    );
    println!("  {}", "-".repeat(92));

    let old = ffprobe.and_then(|p| old_media(p, file));

    // 时长
    match old {
        Some(m) => row(
            "时长(秒)",
            format!("{:.6}", m.duration),
            format!("{:.3}", banner.duration),
            (m.duration - banner.duration).abs() < 0.01,
        ),
        None => row_new("时长(秒)", format!("{:.3}", banner.duration)),
    }

    // 帧率：新侧有两个来源，showinfo 的精确值和横幅的两位小数
    let new_fps = match exact {
        Some(r) => format!("{}（横幅 {}）", fmt_rat(r), fmt_fps(banner.fps())),
        None => format!("-（横幅 {}）", fmt_fps(banner.fps())),
    };
    match old {
        Some(m) => row(
            "帧率",
            format!("{} avg={}", fmt_fps(m.raw_fps), fmt_fps(m.avg_fps)),
            new_fps,
            exact.map(|r| (r.as_f64() - m.raw_fps).abs() < 1e-6).unwrap_or(false),
        ),
        None => row_new("帧率", new_fps),
    }

    // 宽高
    match old {
        Some(m) => row(
            "宽x高",
            if m.width > 0 {
                format!("{}x{}", m.width, m.height)
            } else {
                "-".into()
            },
            if banner.width > 0 {
                format!("{}x{}", banner.width, banner.height)
            } else {
                "-".into()
            },
            banner.width == m.width && banner.height == m.height,
        ),
        None => row_new(
            "宽x高",
            if banner.width > 0 {
                format!("{}x{}", banner.width, banner.height)
            } else {
                "-".into()
            },
        ),
    }

    // 音频编码名
    let new_a = banner.audio_codec.clone();
    match ffprobe.map(|p| old_audio(p, file)) {
        Some(old_a) => row(
            "音频编码名",
            old_a.clone().unwrap_or_else(|| "-".into()),
            new_a.clone().unwrap_or_else(|| "-".into()),
            old_a == new_a,
        ),
        None => row_new("音频编码名", new_a.unwrap_or_else(|| "-".into())),
    }

    // 包数：ffprobe 的 -count_packets vs 原语 (D)
    let new_packets = probe::packet_count(ffmpeg, file).unwrap_or(0);
    match ffprobe {
        Some(p) => {
            let old_pkt = old_packets(p, file);
            row(
                "视频包数",
                old_pkt.to_string(),
                new_packets.to_string(),
                old_pkt == new_packets,
            );
        }
        None => row_new("视频包数", new_packets.to_string()),
    }

    // 帧表：这一项 ffprobe 没有，只有新侧
    match &table {
        Some(t) => {
            row_new(
                "帧表",
                format!(
                    "{} 帧，time_base {}，跨度 {:.3}s",
                    t.len(),
                    fmt_rat(t.timebase),
                    t.span_secs()
                ),
            );
        }
        None => row_new("帧表", "建不出来（pts 全是 NOPTS，或是坏文件）".into()),
    }

    // 总帧数：ffprobe 的 nb_frames vs 新侧的取法。
    // 新侧按**主逻辑的规则**算——VFR 用表长，CFR 用"时长 × 精确帧率"的估算。
    // 所以下面的帧表虽然是每次都建的，CFR 文件这里不会拿它当帧数。
    let new_nb = if vfr {
        table.as_ref().map(|t| t.len() as i64).unwrap_or(0)
    } else {
        match exact {
            Some(r) if banner.duration > 0.0 => (banner.duration * r.as_f64()).round() as i64,
            _ => 0,
        }
    };
    match old {
        Some(m) => row(
            "总帧数",
            if m.nb_frames > 0 {
                m.nb_frames.to_string()
            } else {
                "-".into()
            },
            if new_nb > 0 {
                format!("{new_nb}")
            } else {
                "-".into()
            },
            m.nb_frames <= 0 || new_nb == m.nb_frames,
        ),
        None => row_new("总帧数", new_nb.to_string()),
    }

    // VFR 两级判据
    println!();
    println!(
        "  一级筛子（横幅 avg vs tbr，两位小数）：{} / {}  → {}",
        banner.avg_fps,
        banner.r_fps,
        if suspects { "可疑 VFR" } else { "CFR" }
    );
    match (&table, exact) {
        (Some(t), Some(r)) => {
            let frames = t.len();
            let span = t.span_secs();
            let expect = span * r.as_f64();
            let rel = if expect > 0.0 {
                ((frames - 1) as f64 - expect).abs() / expect
            } else {
                0.0
            };
            println!(
                "  二级判据（{} 帧，跨度 {:.4}s × {}）：期望 {:.2} 个间隔，实得 {}，相对差 {:.2e} → {}",
                frames,
                span,
                fmt_rat(r),
                expect,
                frames - 1,
                rel,
                if vfr { "VFR" } else { "CFR" }
            );
            if suspects && !vfr {
                println!("             （一级报了可疑但二级判是 CFR，主逻辑会丢掉这张表）");
            }
        }
        (None, _) => println!("  二级判据：帧表建不出来（pts 全是 NOPTS，或是坏文件），按 CFR 处理"),
        (_, None) => println!("  二级判据：拿不到精确帧率，判不了"),
    }
}

fn main() {
    // cargo run --example 的工作目录是包根目录，工具就在 tools\ 下
    let ffmpeg = PathBuf::from(r"tools\ffmpeg.exe");
    if !ffmpeg.exists() {
        eprintln!("找不到 {}（请在项目根目录下跑）", ffmpeg.display());
        std::process::exit(1);
    }
    let ffprobe = PathBuf::from(r"tools\ffprobe.exe");
    let ffprobe = ffprobe.exists().then_some(ffprobe);
    if ffprobe.is_none() {
        println!("（tools\\ffprobe.exe 不在，只打新侧）");
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let files: Vec<String> = if args.is_empty() {
        DEFAULT_FILES
            .iter()
            .filter(|f| Path::new(f).exists())
            .map(|f| f.to_string())
            .collect()
    } else {
        args
    };

    if files.is_empty() {
        eprintln!("没有可测的文件");
        std::process::exit(1);
    }

    for f in &files {
        show(&ffmpeg, ffprobe.as_deref(), f);
    }
    println!();
}
