//! 预览改走 ffmpeg 子进程之前的门槛测试（计划里的 P0）。
//!
//! 跑的是真模块：`#[path]` include `preview_media.rs`、`wasapi.rs`、`probe.rs` 本尊，
//! 不另写一份实现。自带一个极简 Slint 窗口，所以"ffmpeg 解码 → 环形缓冲 → 调度 →
//! Slint 显示"整条路是端到端跑通的，不是打桩。
//!
//! 跑法：
//!   cargo run --example preview_ffmpeg_spike -- --ffmpeg tools/ffmpeg.exe --input "tests\测试.mp4"
//!
//! 参数：
//!   --ffmpeg <路径>      默认 tools/ffmpeg.exe
//!   --input <路径>       必填
//!   --start <秒>         起播位置，默认 30
//!   --seconds <秒>       连续播放时长，默认 60
//!   --rss-seconds <秒>   内存观察时长，默认 300；给 0 跳过
//!   --check-frames <n>   showinfo 交叉验证比对几帧，默认 240
//!   --no-window          不开窗口（其余照跑）
//!   --no-audio           强制当无音轨处理
//!
//! 跑完打印门槛表，全过退出码 0。

#[path = "../src/platform/wasapi.rs"]
mod wasapi;
#[path = "../src/services/preview_media.rs"]
mod preview_media;
#[path = "../src/services/probe.rs"]
mod probe;

slint::slint! {
    export component SpikeWindow inherits Window {
        in property <image> preview-frame;
        in property <string> status;
        title: "preview ffmpeg spike";
        width: 640px;
        height: 400px;
        Rectangle {
            background: #000000;
            VerticalLayout {
                Image {
                    source: root.preview-frame;
                    image-fit: contain;
                }
                Text {
                    text: root.status;
                    color: #a0a0a0;
                    font-size: 12px;
                    horizontal-alignment: center;
                    vertical-alignment: center;
                    min-height: 40px;
                }
            }
        }
    }
}

use preview_media::{AudioSpec, Frame, FrameRing, FrameSecs, Session, SourceSpec};
use probe::Framing;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wasapi::AudioEngine;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// PCM 环的上限：48 kHz 立体声 s16 的四秒
const PCM_LIMIT: usize = 48_000 * 2 * 2 * 4;
/// 跑起来头半秒不采调度误差：刚 Start 那几帧本来就不代表稳态
const WARMUP_SECS: f64 = 0.5;
/// 离下一帧还差这么点时间就不再睡，改成自旋等。比 `sleep` 的固定开销大一点，
/// 留出余量来吸收抖动。一帧 41.7 ms 里自旋 1 ms，占一个核的 2% 上下。
const SPIN_SECS: f64 = 0.001;

// ------------------------------------------------------------------ 参数

#[derive(Clone)]
struct Args {
    ffmpeg: String,
    input: String,
    start: f64,
    seconds: f64,
    rss_seconds: f64,
    check_frames: usize,
    no_window: bool,
    no_audio: bool,
}

fn parse_args() -> Args {
    let v: Vec<String> = std::env::args().skip(1).collect();
    let mut a = Args {
        ffmpeg: "tools/ffmpeg.exe".into(),
        input: String::new(),
        start: 30.0,
        seconds: 60.0,
        rss_seconds: 300.0,
        check_frames: 240,
        no_window: false,
        no_audio: false,
    };
    let val = |i: usize| v.get(i + 1).cloned().unwrap_or_default();
    let mut i = 0;
    while i < v.len() {
        match v[i].as_str() {
            "--ffmpeg" => {
                a.ffmpeg = val(i);
                i += 2;
            }
            "--input" => {
                a.input = val(i);
                i += 2;
            }
            "--start" => {
                a.start = val(i).parse().unwrap_or(30.0);
                i += 2;
            }
            "--seconds" => {
                a.seconds = val(i).parse().unwrap_or(60.0);
                i += 2;
            }
            "--rss-seconds" => {
                a.rss_seconds = val(i).parse().unwrap_or(300.0);
                i += 2;
            }
            "--check-frames" => {
                a.check_frames = val(i).parse().unwrap_or(240);
                i += 2;
            }
            "--no-window" => {
                a.no_window = true;
                i += 1;
            }
            "--no-audio" => {
                a.no_audio = true;
                i += 1;
            }
            s => {
                eprintln!("不认识的参数：{s}");
                std::process::exit(2);
            }
        }
    }
    a
}

// ------------------------------------------------------------------ 门槛表

struct Gate {
    name: String,
    /// None 表示这次没测到
    pass: Option<bool>,
    detail: String,
}

fn gate(name: impl Into<String>, pass: Option<bool>, detail: impl Into<String>) -> Gate {
    Gate {
        name: name.into(),
        pass,
        detail: detail.into(),
    }
}

fn verdict(g: &Gate) -> &'static str {
    match g.pass {
        Some(true) => "通过",
        Some(false) => "没过",
        None => "未测",
    }
}

fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = (((s.len() - 1) as f64) * p).round() as usize;
    s[i]
}

fn ms(v: f64) -> String {
    format!("{v:.2} ms")
}

// ------------------------------------------------------------------ 内存

fn rss_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    unsafe {
        K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb);
    }
    c.WorkingSetSize as u64
}

// ------------------------------------------------------------------ 播放器

/// 一条会话里会变的东西，全塞一把锁里，调度器每次 tick 整段拿。
struct Live {
    seek: f64,
    ring: Option<Arc<FrameRing>>,
    /// 等 UI 取走的帧
    slot: Option<Frame>,
}

struct Stats {
    phase: String,
    /// (阶段名, 调度误差毫秒)
    errors: Vec<(String, f64)>,
}

struct AudioPhase {
    underruns: u64,
    silence_frames: u64,
    worst_gap_ms: f64,
    callbacks: u64,
}

struct SeekReport {
    frame0: i64,
    first: Option<Frame>,
    /// seek 指令发出到第一帧可以显示
    total: Duration,
    /// 只算两个子进程起来的耗时
    spawned: Duration,
    /// PCM 攒够一个设备缓冲了
    primed: bool,
}

struct Player {
    ffmpeg: String,
    input: String,
    framing: Framing,
    map: FrameSecs,
    audio_spec: Option<AudioSpec>,
    engine: Option<Arc<AudioEngine>>,
    sess: Mutex<Option<Session>>,
    live: Mutex<Live>,
    stats: Mutex<Stats>,
    sched: Mutex<Option<JoinHandle<()>>>,
    render: Mutex<Option<JoinHandle<()>>>,
    paused: AtomicBool,
    stop: AtomicBool,
    done: AtomicBool,
    /// 调度循环跑了几圈 / 真的挑了帧几次。诊断用。
    iters: AtomicU64,
    picks: AtomicU64,
    seek_bits: AtomicU64,
    /// 无音轨时用的墙钟锚点：now_local = (t_ref 起点到现在) - wall_base
    t_ref: Instant,
    wall_base_ns: AtomicU64,
    frozen_local_ns: AtomicU64,
}

impl Player {
    fn new(args: &Args, framing: Framing, engine: Option<Arc<AudioEngine>>) -> Arc<Player> {
        let map: FrameSecs = {
            let f = framing.clone();
            Arc::new(move |i: i64| f.frame_to_secs(i))
        };
        let audio_spec = engine.as_ref().map(|e| {
            let f = e.format();
            AudioSpec {
                rate: f.rate,
                channels: f.channels,
                sample: f.ffmpeg_sample().to_string(),
            }
        });
        let t_ref = Instant::now();
        Arc::new(Player {
            ffmpeg: args.ffmpeg.clone(),
            input: args.input.clone(),
            framing,
            map,
            audio_spec,
            engine,
            sess: Mutex::new(None),
            live: Mutex::new(Live {
                seek: 0.0,
                ring: None,
                slot: None,
            }),
            stats: Mutex::new(Stats {
                phase: String::new(),
                errors: Vec::new(),
            }),
            sched: Mutex::new(None),
            render: Mutex::new(None),
            paused: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            done: AtomicBool::new(false),
            iters: AtomicU64::new(0),
            picks: AtomicU64::new(0),
            seek_bits: AtomicU64::new(0.0f64.to_bits()),
            t_ref,
            wall_base_ns: AtomicU64::new(t_ref.elapsed().as_nanos() as u64),
            frozen_local_ns: AtomicU64::new(0),
        })
    }

    fn has_audio(&self) -> bool {
        self.engine.is_some()
    }

    fn set_phase(&self, p: &str) {
        self.stats.lock().unwrap().phase = p.to_string();
    }

    /// 会话内的媒体时间。
    fn now_local(&self) -> f64 {
        match &self.engine {
            Some(e) => e.local_us() as f64 / 1e6,
            None => {
                let now = self.t_ref.elapsed().as_nanos() as u64;
                (now.saturating_sub(self.wall_base_ns.load(Ordering::Acquire))) as f64 / 1e9
            }
        }
    }

    fn now_abs(&self) -> f64 {
        f64::from_bits(self.seek_bits.load(Ordering::Acquire)) + self.now_local()
    }

    fn finished(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    fn take_slot(&self) -> Option<Frame> {
        self.live.lock().unwrap().slot.take()
    }

    fn audio_reset(&self) {
        if let Some(e) = &self.engine {
            e.underruns.store(0, Ordering::Relaxed);
            e.silence_frames.store(0, Ordering::Relaxed);
            e.worst_gap_ns.store(0, Ordering::Relaxed);
            e.callbacks.store(0, Ordering::Relaxed);
        }
    }

    fn audio_take(&self) -> Option<AudioPhase> {
        let e = self.engine.as_ref()?;
        Some(AudioPhase {
            underruns: e.underruns.load(Ordering::Relaxed),
            silence_frames: e.silence_frames.load(Ordering::Relaxed),
            worst_gap_ms: e.worst_gap_ns.load(Ordering::Relaxed) as f64 / 1e6,
            callbacks: e.callbacks.load(Ordering::Relaxed),
        })
    }

    fn pause(&self) {
        if let Some(e) = &self.engine {
            e.pause();
        } else {
            let now = self.t_ref.elapsed().as_nanos() as u64;
            let local = now.saturating_sub(self.wall_base_ns.load(Ordering::Acquire));
            self.frozen_local_ns.store(local, Ordering::Release);
        }
        self.paused.store(true, Ordering::Release);
    }

    fn resume(&self) {
        if let Some(e) = &self.engine {
            e.start();
        } else {
            let f = self.frozen_local_ns.load(Ordering::Acquire);
            let now = self.t_ref.elapsed().as_nanos() as u64;
            self.wall_base_ns.store(now.saturating_sub(f), Ordering::Release);
        }
        self.paused.store(false, Ordering::Release);
    }

    /// 跳到 `t` 秒。两个子进程重建、设备换源、先攒后启动。
    fn seek(&self, t: f64) -> Result<SeekReport, String> {
        let t0 = Instant::now();
        let frame0 = self.framing.first_frame_at_or_after(t);

        // 1. 先把环摘掉，调度器立刻停手，免得它拿着旧环又吐一帧出去
        {
            let mut g = self.live.lock().unwrap();
            g.ring = None;
            g.slot = None;
            g.seek = t;
        }
        self.seek_bits.store(t.to_bits(), Ordering::Release);
        self.done.store(false, Ordering::Release);

        // 2. 设备停流、把残留的旧 PCM 冲掉（Reset 只在停止状态合法，顺序不能反）
        if let Some(e) = &self.engine {
            e.reset();
        }

        // 3. 扔掉旧会话（会杀子进程、join 读数线程）
        let old = self.sess.lock().unwrap().take();
        drop(old);

        // 4. 起新会话
        let spec = SourceSpec {
            ffmpeg: self.ffmpeg.clone(),
            input: self.input.clone(),
            seek: t,
            frame0,
            frame_secs: self.map.clone(),
            audio: self.audio_spec.clone(),
        };
        let sess = Session::start(spec, PCM_LIMIT).map_err(|e| e.to_string())?;
        let ring = sess.frames().clone();
        let pcm = sess.pcm().cloned();
        let spawned = t0.elapsed();

        // 5. 换源
        if let (Some(e), Some(p)) = (&self.engine, pcm) {
            let fill: wasapi::PcmFill = Arc::new(move |dst, need| unsafe { p.fill(dst, need) });
            e.set_source(Some(fill));
        }
        *self.sess.lock().unwrap() = Some(sess);

        // 6. 装上环，本地时间归零
        {
            let mut g = self.live.lock().unwrap();
            g.ring = Some(ring.clone());
        }
        self.wall_base_ns
            .store(self.t_ref.elapsed().as_nanos() as u64, Ordering::Release);
        self.frozen_local_ns.store(0, Ordering::Release);

        // 7. 先攒够一个设备缓冲再 Start
        let primed = match &self.engine {
            Some(e) => e.prime_and_start(Duration::from_millis(400)),
            None => true,
        };

        // 8. 等第一帧
        let first = ring.wait_for_index(frame0, Duration::from_millis(2000));

        Ok(SeekReport {
            frame0,
            first,
            total: t0.elapsed(),
            spawned,
            primed,
        })
    }

    /// 暂停状态下走一帧。
    fn step(&self, delta: i64) -> Option<Frame> {
        let mut g = self.live.lock().unwrap();
        let ring = g.ring.clone()?;
        let f = ring.get(ring.cursor() + delta)?;
        ring.set_cursor(f.index);
        g.slot = Some(f.clone());
        Some(f)
    }

    fn spawn_threads(self: &Arc<Self>) {
        if let Some(e) = &self.engine {
            let e2 = e.clone();
            let h = std::thread::Builder::new()
                .name("spike-render".into())
                .spawn(move || {
                    // 每个碰音频接口的线程都自己初始化一次 COM，都按 MTA。
                    // 主线程不能碰：winit 建窗口时会按 STA 调 OleInitialize，
                    // 已经初始化成 MTA 的线程上那个调用直接失败。
                    if let Err(err) = wasapi::init_com() {
                        eprintln!("渲染线程 init_com 失败：{err}");
                        return;
                    }
                    AudioEngine::render_loop(&e2);
                    wasapi::uninit_com();
                })
                .expect("起渲染线程");
            *self.render.lock().unwrap() = Some(h);
        }
        let me = self.clone();
        let h = std::thread::Builder::new()
            .name("spike-sched".into())
            .spawn(move || me.sched_loop())
            .expect("起调度线程");
        *self.sched.lock().unwrap() = Some(h);
    }

    /// 出帧调度。时钟走到某帧的时间点就把它丢进 slot，等 UI 定时器来取。
    ///
    /// 睡到点上的精度是这里的关键：如果改成"每 30 ms 由 UI 线程挑一帧"，
    /// 挑的是"现在为止最新的那帧"，误差就是一个 tick 的量级（p50 十几毫秒），
    /// 音画同步那条门槛根本够不着。所以调度自己起一条线程，按帧的点叫醒。
    fn sched_loop(self: Arc<Self>) {
        // 这条线程要读 IAudioClock 的位置，也得自己初始化 COM
        if self.engine.is_some() && wasapi::init_com().is_err() {
            eprintln!("调度线程 init_com 失败");
            return;
        }
        loop {
            if self.stop.load(Ordering::Acquire) {
                return;
            }
            if self.paused.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            self.iters.fetch_add(1, Ordering::Relaxed);

            let raw_local = self.now_local();
            let mut g = self.live.lock().unwrap();
            let Some(ring) = g.ring.clone() else {
                drop(g);
                std::thread::sleep(Duration::from_millis(2));
                continue;
            };
            let seek = g.seek;

            let next_local = (self.map)(ring.cursor() + 1) - seek;
            // 无音轨时用墙钟。但不能让它跑到下一帧前头去：解码卡住的时候
            // 位置自己往前爬会把内容整段跳过去。
            let local = if self.has_audio() {
                raw_local
            } else {
                raw_local.min(next_local)
            };

            if next_local > local {
                let d = next_local - local;
                drop(g);
                // 一次睡掉大头，最后这一小段自己转。
                //
                // 不能像原来那样"每次只睡 d 的一半"：本机实测 `sleep` 有约 0.5 ms 的
                // 固定开销（`sleep(1ms)` 实际 1.53 ms，`sleep(200µs)` 实际 0.53 ms），
                // 一帧拆成十来次小睡就把 41.7 ms 累积成 50 ms 以上，于是每几帧就整个
                // 跳过一帧。跳过的那帧不会体现成"排期误差"（下一帧就在那儿，误差照样
                // 很小），只会体现成帧率掉下去——正是门槛第 8 条在盯的东西。
                if d > SPIN_SECS {
                    std::thread::sleep(Duration::from_secs_f64(d - SPIN_SECS));
                } else {
                    std::hint::spin_loop();
                }
                continue;
            }

            let now_abs = seek + local;
            match ring.take_due(now_abs) {
                Some(f) => {
                    let err_ms = (now_abs - f.secs) * 1000.0;
                    g.slot = Some(f);
                    drop(g);
                    self.picks.fetch_add(1, Ordering::Relaxed);
                    // 刚起来头半秒不采：那时候管道还没走顺
                    if local > WARMUP_SECS {
                        let mut s = self.stats.lock().unwrap();
                        let p = s.phase.clone();
                        s.errors.push((p, err_ms));
                    }
                }
                None => {
                    let done = ring.is_eof() && ring.back_index().map_or(true, |b| b <= ring.cursor());
                    drop(g);
                    if done {
                        // 后面没有了，最后那帧留在屏上
                        self.done.store(true, Ordering::Release);
                        std::thread::sleep(Duration::from_millis(20));
                    } else {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        }
    }

    fn stop_all(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.sched.lock().unwrap().take() {
            let _ = h.join();
        }
        if let Some(e) = &self.engine {
            e.shutdown();
        }
        if let Some(h) = self.render.lock().unwrap().take() {
            let _ = h.join();
        }
        let old = self.sess.lock().unwrap().take();
        drop(old);
    }
}

// ------------------------------------------------------------------ ffmpeg 旁路

/// 把 `video_args` 那条链原样拿来，尾部接上 `select`，抽出第 n 帧。
///
/// 用的是同一个 `video_args`，所以滤镜链一定是同一条；参考帧和播放帧不一致
/// 只可能是解码端的问题。
fn reference_frame(ffmpeg: &str, input: &str, seek: f64, n: i64) -> Result<Vec<u8>, String> {
    let mut args = preview_media::video_args(input, seek);
    let vi = args
        .iter()
        .position(|a| a == "-vf")
        .ok_or("video_args 里没有 -vf")?
        + 1;
    args[vi] = format!("{},select=eq(n\\,{})", args[vi], n);
    let oi = args
        .iter()
        .position(|a| a == "pipe:1")
        .ok_or("video_args 里没有 pipe:1")?;
    args.insert(oi, "-frames:v".into());
    args.insert(oi + 1, "1".into());

    let mut child = Command::new(ffmpeg)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("起 ffmpeg 失败：{e}"))?;

    let mut out = child.stdout.take().ok_or("拿不到 stdout")?;
    let mut buf = vec![0u8; preview_media::FRAME_BYTES];
    let r = out.read_exact(&mut buf);
    if let Err(e) = r {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("读参考帧失败：{e}"));
    }
    // 读完一帧就把进程收掉，不然它可能会卡在写下一帧上
    let _ = child.kill();
    let _ = child.wait();
    Ok(buf)
}

/// showinfo 旁路：把时间基强行设成 1/1000000 再打 pts。
///
/// 不设的话 showinfo 的 `pts_time` 是 `%.6g`，跳到 120 秒之后只有毫秒精度，
/// 正好压在 1 ms 的容差线上；`pts` 是整数，换成微秒就没有这个问题。
fn showinfo_times(ffmpeg: &str, input: &str, seek: f64, frames: usize) -> Result<Vec<f64>, String> {
    let mut args = preview_media::video_args(input, seek);
    let vi = args
        .iter()
        .position(|a| a == "-vf")
        .ok_or("video_args 里没有 -vf")?
        + 1;
    args[vi] = format!("{},settb=tb=1/1000000,showinfo", args[vi]);
    // 把日志级别提上去，showinfo 才打得出东西
    if let Some(li) = args.iter().position(|a| a == "-loglevel") {
        args[li + 1] = "info".into();
    }
    // 从 `-f rawvideo` 那对参数开始全砍掉，换成写进 null muxer。
    // 留着的话后面那个 `-f null` 虽然会把它盖掉，但两条 `-f` 摆在一起太容易看错。
    let fi = args
        .iter()
        .position(|a| a == "-f")
        .ok_or("video_args 里没有 -f")?;
    args.truncate(fi);
    args.extend([
        "-frames:v".into(),
        frames.to_string(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]);

    let out = Command::new(ffmpeg)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("起 ffmpeg 失败：{e}"))?;

    let s = String::from_utf8_lossy(&out.stderr);
    let mut v = Vec::new();
    for line in s.lines() {
        let Some(i) = line.find("] n:") else { continue };
        let rest = &line[i + 4..];
        // `pts:` 前面有个空格；`pts_time:` 前面也是空格，但那是 " pts_time"，
        // 所以找 " pts:" 不会撞上
        let Some(p) = rest.find(" pts:") else { continue };
        let after = rest[p + 5..].trim_start();
        let num: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        if let Ok(pts) = num.parse::<i64>() {
            v.push(pts as f64 / 1e6);
        }
    }
    if v.is_empty() {
        return Err(format!(
            "showinfo 一行都没解析出来，stderr 头几行：\n{}",
            s.lines().take(6).collect::<Vec<_>>().join("\n")
        ));
    }
    Ok(v)
}

// ------------------------------------------------------------------ 测试流程

/// 在一个自己初始化成 MTA 的线程上开音频设备，然后把句柄交回来。
///
/// 主线程不能干这件事：winit 建窗口时按 STA 调 `OleInitialize`，一个已经初始化
/// 成 MTA 的线程上那个调用会直接失败并 panic。产品里 `setup_preview` 跑在主线程上，
/// 所以 P1 也得照这个形状来——音频设备归预览自己的线程所有。
fn open_engine_on_own_thread() -> Result<Arc<AudioEngine>, String> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("spike-audio-open".into())
        .spawn(move || {
            let r = wasapi::init_com().and_then(|_| AudioEngine::open());
            let _ = tx.send(r);
        })
        .map_err(|e| format!("起开设备线程失败：{e}"))?;
    rx.recv().map_err(|e| format!("开设备线程没回话：{e}"))?
}

/// 当场量一下 `sleep` 的真实粒度。排期能到多准，全看这个数。
fn sleep_probe(label: &str, want: Duration, n: usize) {
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        std::thread::sleep(want);
        v.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    println!(
        "      [诊断] sleep({label}) 实测 p50 {} / p99 {} / max {}",
        ms(pct(&v, 0.5)),
        ms(pct(&v, 0.99)),
        ms(v.iter().cloned().fold(0.0, f64::max))
    );
}

/// 音频时钟多久动一次。步长比一帧还大的话，再准的排期也白搭。
fn clock_probe(p: &Player, secs: f64) {
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    let mut steps: Vec<f64> = Vec::new();
    let mut last = p.now_local();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_micros(100));
        let now = p.now_local();
        if now > last {
            steps.push((now - last) * 1000.0);
            last = now;
        }
    }
    if steps.is_empty() {
        println!("      [诊断] 音频时钟：{}s 内一次都没动", secs);
        return;
    }
    println!(
        "      [诊断] 音频时钟：{:.2}s 内动了 {} 次，步长 p50 {} / p99 {} / max {}",
        secs,
        steps.len(),
        ms(pct(&steps, 0.5)),
        ms(pct(&steps, 0.99)),
        ms(steps.iter().cloned().fold(0.0, f64::max))
    );
}

fn sleep_play(p: &Player, secs: f64) {
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < deadline {
        if p.finished() {
            // 播完了，让它停一会儿把剩下的状态收干净
            std::thread::sleep(Duration::from_millis(200));
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn take_errors(p: &Player, phase: &str) -> Vec<f64> {
    let mut s = p.stats.lock().unwrap();
    let mut v = Vec::new();
    s.errors.retain(|(ph, e)| {
        if ph == phase {
            v.push(*e);
            false
        } else {
            true
        }
    });
    v
}

fn gate_sync(name: &str, errs: &[f64]) -> Gate {
    if errs.is_empty() {
        return gate(format!("{name} / 调度误差"), None, "没采到样本");
    }
    let p50 = pct(errs, 0.50);
    let p99 = pct(errs, 0.99);
    let max = errs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let ok = p50 <= 5.0 && p99 <= 20.0 && max <= 50.0;
    gate(
        format!("{name} / 调度误差 clock-pts"),
        Some(ok),
        format!("n={} p50 {} / p99 {} / max {}", errs.len(), ms(p50), ms(p99), ms(max)),
    )
}

fn gate_audio(name: &str, a: &Option<AudioPhase>, primed: bool) -> Gate {
    let Some(a) = a else {
        return gate(format!("{name} / 不注静音"), None, "没开音频设备");
    };
    let ok = primed && a.underruns == 0 && a.silence_frames <= 480 && a.worst_gap_ms <= 20.0;
    gate(
        format!("{name} / 不注静音"),
        Some(ok),
        format!(
            "攒够缓冲={} underrun={} 补零采样={}（{:.1} ms） 最长缺口 {} 回调 {} 次",
            primed,
            a.underruns,
            a.silence_frames,
            a.silence_frames as f64 / 48.0,
            ms(a.worst_gap_ms),
            a.callbacks
        ),
    )
}

fn run_phases(
    p: &Arc<Player>,
    args: &Args,
    framing: &Framing,
    shown: &Arc<AtomicU64>,
    set_ms: &Arc<Mutex<Vec<f64>>>,
    status: &Arc<Mutex<String>>,
) -> Vec<Gate> {
    let mut gates: Vec<Gate> = Vec::new();
    let fps = framing.fps_f64();
    let set = |s: &Arc<Mutex<String>>, t: &str| *s.lock().unwrap() = t.to_string();

    // ---------- 起播 ----------
    set(status, "起播");
    p.set_phase("起播");
    p.audio_reset();
    let r = match p.seek(args.start) {
        Ok(r) => r,
        Err(e) => {
            gates.push(gate("起播", Some(false), e));
            return gates;
        }
    };
    gates.push(gate(
        "起播 / 跳转延迟",
        Some(r.total <= Duration::from_millis(400)),
        format!(
            "spawn {} / 首帧可显示 {}（帧号 {}）",
            ms(r.spawned.as_secs_f64() * 1000.0),
            ms(r.total.as_secs_f64() * 1000.0),
            r.frame0
        ),
    ));

    // ---------- 连续播放 ----------
    set(status, "连续播放");
    p.set_phase("连续");
    p.audio_reset();
    let s0 = shown.load(Ordering::Relaxed);
    let i0 = p.iters.load(Ordering::Relaxed);
    let k0 = p.picks.load(Ordering::Relaxed);
    clock_probe(p, 0.3);
    let t0 = Instant::now();
    {
        let mut last = (0usize, 0u64);
        while t0.elapsed().as_secs_f64() < args.seconds && !p.finished() {
            std::thread::sleep(Duration::from_millis(500));
            let g = p.live.lock().unwrap();
            let (cur, back, eof) = match &g.ring {
                Some(r) => (r.cursor(), r.back_index().unwrap_or(-1), r.is_eof()),
                None => (i64::MIN, -1, false),
            };
            drop(g);
            let picks = p.picks.load(Ordering::Relaxed) as usize;
            let sh = shown.load(Ordering::Relaxed);
            println!(
                "      [采样] 墙钟 {:.2}s 时钟 {:.3}s 游标 {} 环尾 {} eof={} 挑帧 +{} 送帧 +{}",
                t0.elapsed().as_secs_f64(),
                p.now_local(),
                cur,
                back,
                eof,
                picks - last.0,
                sh - last.1
            );
            last = (picks, sh);
        }
    }
    let wall = t0.elapsed().as_secs_f64();
    let s1 = shown.load(Ordering::Relaxed);
    let i1 = p.iters.load(Ordering::Relaxed);
    let k1 = p.picks.load(Ordering::Relaxed);
    println!(
        "      [诊断] 调度循环 {:.0} 圈/秒，挑帧 {:.1} 次/秒，送 Slint {:.1} 帧/秒，墙钟 {:.2}s",
        (i1 - i0) as f64 / wall,
        (k1 - k0) as f64 / wall,
        (s1 - s0) as f64 / wall,
        wall
    );
    let errs = take_errors(p, "连续");
    gates.push(gate_sync("连续播放", &errs));
    gates.push(gate_audio("连续播放", &p.audio_take(), true));
    if fps > 0.0 {
        let rate = (s1 - s0) as f64 / args.seconds;
        if args.no_window {
            gates.push(gate("连续播放 / 送到 Slint 的帧率", None, "没开窗口，没人取帧"));
        } else {
            gates.push(gate(
                "连续播放 / 送到 Slint 的帧率",
                Some(rate >= 0.9 * fps),
                format!("{rate:.2} fps（源 {fps:.3} fps，{s1} 帧）"),
            ));
        }
    }

    // ---------- 跳转：帧准确 + 延迟 + 不注静音 ----------
    for t in [0.0, args.start, 120.0] {
        let name = format!("跳转 {t:.0}s");
        set(status, &name);
        p.set_phase(&name);
        p.audio_reset();
        let r = match p.seek(t) {
            Ok(r) => r,
            Err(e) => {
                gates.push(gate(&name, Some(false), e));
                continue;
            }
        };
        gates.push(gate(
            format!("{name} / 跳转延迟"),
            Some(r.total <= Duration::from_millis(400)),
            format!(
                "spawn {} / 首帧 {}",
                ms(r.spawned.as_secs_f64() * 1000.0),
                ms(r.total.as_secs_f64() * 1000.0)
            ),
        ));

        // 帧准确：拿解码出的第 0 帧和第 37 帧跟 select 旁路逐字节比
        let mut detail = Vec::new();
        let mut ok = true;
        match &r.first {
            None => {
                ok = false;
                detail.push("首帧没等到".to_string());
            }
            Some(_) => {
                for n in [0i64, 37] {
                    let ours = match p.live.lock().unwrap().ring.clone() {
                        Some(ring) => ring.get(r.frame0 + n).map(|mut fr| {
                            (
                                fr.pixels.make_mut_bytes().to_vec(),
                                fr.secs,
                            )
                        }),
                        None => None,
                    };
                    let Some((ours, secs)) = ours else {
                        ok = false;
                        detail.push(format!("第 {n} 帧不在环里"));
                        continue;
                    };
                    match reference_frame(&args.ffmpeg, &args.input, t, n) {
                        Ok(want) => {
                            let same = want == ours;
                            ok &= same;
                            detail.push(format!(
                                "第{n}帧({secs:.4}s) {}",
                                if same { "一致" } else { "不一致" }
                            ));
                        }
                        Err(e) => {
                            ok = false;
                            detail.push(format!("第 {n} 帧参考解码失败：{e}"));
                        }
                    }
                }
            }
        }
        gates.push(gate(format!("{name} / 跳转帧准确"), Some(ok), detail.join("，")));

        // 跳完接着播一会儿，看有没有注静音
        sleep_play(p, 3.0);
        gates.push(gate_audio(&name, &p.audio_take(), r.primed));
        let errs = take_errors(p, &name);
        gates.push(gate_sync(&name, &errs));
    }

    // ---------- 逐帧 ----------
    set(status, "逐帧");
    p.set_phase("逐帧");
    sleep_play(p, 5.0);
    p.pause();
    std::thread::sleep(Duration::from_millis(200));
    let mut step_ms: Vec<f64> = Vec::new();
    let mut back_ok = true;
    for _ in 0..20 {
        let t = Instant::now();
        if p.step(-1).is_none() {
            back_ok = false;
            break;
        }
        step_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let back_max = step_ms.iter().cloned().fold(0.0f64, f64::max);
    let mut fwd_ms: Vec<f64> = Vec::new();
    let mut fwd_ok = true;
    for _ in 0..20 {
        let t = Instant::now();
        if p.step(1).is_none() {
            fwd_ok = false;
            break;
        }
        fwd_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let fwd_max = fwd_ms.iter().cloned().fold(0.0f64, f64::max);
    gates.push(gate(
        "逐帧 / 走缓冲",
        Some(back_ok && fwd_ok && back_max <= 2.0 && fwd_max <= 2.0),
        format!(
            "往回 {} 次最慢 {}（{}–{}），往前 {} 次最慢 {}（{}–{}）",
            step_ms.len(),
            ms(back_max),
            ms(step_ms.iter().cloned().fold(f64::INFINITY, f64::min)),
            ms(step_ms.iter().cloned().fold(0.0, f64::max)),
            fwd_ms.len(),
            ms(fwd_max),
            ms(fwd_ms.iter().cloned().fold(f64::INFINITY, f64::min)),
            ms(fwd_ms.iter().cloned().fold(0.0, f64::max)),
        ),
    ));
    p.resume();

    // ---------- 显示链路：set 属性开销 ----------
    {
        let v = set_ms.lock().unwrap();
        if v.is_empty() {
            gates.push(gate("显示链路 / set 帧开销", None, "一次都没送到"));
        } else {
            let max = v.iter().cloned().fold(0.0f64, f64::max);
            let p50 = pct(&v, 0.5);
            let p99 = pct(&v, 0.99);
            gates.push(gate(
                "显示链路 / set 帧开销",
                Some(max <= 1.0),
                format!("n={} p50 {} / p99 {} / max {}", v.len(), ms(p50), ms(p99), ms(max)),
            ));
        }
    }

    // ---------- 帧时间轴交叉验证 ----------
    set(status, "showinfo 交叉验证");
    let t = args.start;
    let f0 = framing.first_frame_at_or_after(t);
    match showinfo_times(&args.ffmpeg, &args.input, t, args.check_frames) {
        Err(e) => gates.push(gate("帧时间轴 / showinfo 交叉", None, e)),
        Ok(times) => {
            let base_ours = framing.frame_to_secs(f0);
            let base_ref = times[0];
            let mut worst = 0.0f64;
            let mut n = 0usize;
            for (i, ref_secs) in times.iter().enumerate() {
                let ours = framing.frame_to_secs(f0 + i as i64);
                let d = ((ours - base_ours) - (ref_secs - base_ref)).abs();
                if d > worst {
                    worst = d;
                }
                n += 1;
            }
            // showinfo 那边的时间戳从 0 起（-ss 是输入选项，输出时间被平移过），
            // 所以只能比"相邻差值"。平移量本身也应该正好是 seek，顺带看一眼。
            let shift = base_ours - base_ref;
            gates.push(gate(
                "帧时间轴 / showinfo 交叉",
                Some(worst <= 0.001),
                format!(
                    "{} 帧，最大偏差 {}；常量平移 {:.6} s（seek {:.6} s，差 {:.6} s）",
                    n,
                    ms(worst * 1000.0),
                    shift,
                    t,
                    shift - t
                ),
            ));
        }
    }

    // ---------- 内存 ----------
    if args.rss_seconds >= 5.0 {
        set(status, "长播看内存");
        p.set_phase("长播");
        let before = rss_bytes();
        let deadline = Instant::now() + Duration::from_secs_f64(args.rss_seconds);
        let mut next_seek = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if p.finished() {
                let _ = p.seek(0.0);
            }
            if Instant::now() >= next_seek {
                next_seek = Instant::now() + Duration::from_secs(20);
                let cur = p.now_abs();
                let _ = p.seek((cur + 10.0).min(120.0));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let after = rss_bytes();
        let grow = after.saturating_sub(before) as f64 / (1024.0 * 1024.0);
        gates.push(gate(
            "内存有界",
            Some(grow <= 64.0),
            format!(
                "{:.0} 秒增长 {grow:.1} MiB（{:.1} → {:.1} MiB）",
                args.rss_seconds,
                before as f64 / 1048576.0,
                after as f64 / 1048576.0
            ),
        ));
    }

    gates
}

// ------------------------------------------------------------------ main

fn main() {
    let args = parse_args();
    if args.input.is_empty() {
        eprintln!("缺少 --input");
        std::process::exit(2);
    }

    println!("=== 预览 ffmpeg 门槛测试 ===");
    println!("ffmpeg : {}", args.ffmpeg);
    println!("input  : {}", args.input);
    println!();

    // 这两条是计划里的风险项之一：帧要在工作线程上解码、在主线程上交给 Slint，
    // 中间隔着 slot。类型不 Send 的话编译期就该炸。
    fn assert_send<T: Send>() {}
    assert_send::<slint::SharedPixelBuffer<slint::Rgb8Pixel>>();
    assert_send::<Frame>();

    wasapi::raise_timer_resolution();
    sleep_probe("200µs", Duration::from_micros(200), 500);
    sleep_probe("1ms", Duration::from_millis(1), 500);
    sleep_probe("2ms", Duration::from_millis(2), 300);
    sleep_probe("5ms", Duration::from_millis(5), 200);

    // ---- 探测 ----
    let stop = AtomicBool::new(false);
    let t_probe = Instant::now();
    let pr = probe::probe(Path::new(&args.ffmpeg), &args.input, &stop, &mut |_pct| {});
    println!(
        "探测：打开={} 时长={:.3}s {}x{} 帧率={}/{} 时长估算 {} 帧  音频={:?}",
        pr.opened,
        pr.duration,
        pr.width,
        pr.height,
        pr.fps.num,
        pr.fps.den,
        pr.nb_frames,
        pr.audio_codec
    );
    match &pr.framing {
        Framing::Cfr(r) => println!("      framing: CFR {}/{}", r.num, r.den),
        Framing::Vfr { table, .. } => println!("      framing: VFR，表长 {}", table.len()),
        Framing::Unknown => println!("      framing: 拿不到"),
    }
    println!("      （探针耗时 {}）", ms(t_probe.elapsed().as_secs_f64() * 1000.0));
    if !pr.opened {
        eprintln!("文件打不开，退出");
        std::process::exit(1);
    }
    if !pr.framing.is_valid() {
        eprintln!("帧率拿不到，跳转帧准确和帧时间轴两条门槛测不了");
    }
    println!();

    // ---- 音频设备 ----
    // 这几条在流程线程起跑之前就算出来了，攒着，等门槛表一起打印。
    let mut audio_gates: Vec<Gate> = Vec::new();
    let want_audio = !args.no_audio && pr.audio_codec.is_some();
    let engine = if want_audio {
        // 开设备这件事本身就碰 COM，而主线程要留给 winit（它按 STA 调 OleInitialize）。
        // 所以设备在一个自己初始化成 MTA 的临时线程上开，开完把句柄交回来。
        match open_engine_on_own_thread() {
            Ok(e) => {
                let f = e.format();
                match e.negotiation() {
                    wasapi::Negotiation::Plan => println!(
                        "音频：计划格式被接受 —— {} / 缓冲 {:.1} ms（{} 帧）",
                        f.describe(),
                        e.buffer_ms(),
                        e.buffer_frames()
                    ),
                    wasapi::Negotiation::MixFormat(why) => println!(
                        "音频：回落到设备混音格式 —— {} / 缓冲 {:.1} ms（{} 帧）\n      理由：{}",
                        f.describe(),
                        e.buffer_ms(),
                        e.buffer_frames(),
                        why
                    ),
                }
                let spec = AudioSpec {
                    rate: f.rate,
                    channels: f.channels,
                    sample: f.ffmpeg_sample().to_string(),
                };
                let line = preview_media::audio_args(&args.input, args.start, &spec);
                println!("      音频命令行：ffmpeg {}", line.join(" "));
                let args_ok = line
                    .windows(2)
                    .any(|w| w[0] == "-ar" && w[1] == f.rate.to_string())
                    && line
                        .windows(2)
                        .any(|w| w[0] == "-ac" && w[1] == f.channels.to_string());
                audio_gates.push(gate(
                    "格式协商 / 协商结果用进命令行",
                    Some(args_ok),
                    format!(
                        "{}：{} Hz / {} 声道 / {}（回退分支 {}）",
                        match e.negotiation() {
                            wasapi::Negotiation::Plan => "计划格式",
                            wasapi::Negotiation::MixFormat(_) => "设备混音格式",
                        },
                        f.rate,
                        f.channels,
                        f.ffmpeg_sample(),
                        match e.negotiation() {
                            wasapi::Negotiation::Plan => "本次没走到，本机计划格式被直接接受",
                            wasapi::Negotiation::MixFormat(_) => "本次走的就是它",
                        }
                    ),
                ));
                Some(e)
            }
            Err(e) => {
                eprintln!("音频设备开不了：{e}");
                audio_gates.push(gate("格式协商", Some(false), e));
                None
            }
        }
    } else {
        println!("音频：不开（{}）", if args.no_audio { "命令行指定" } else { "没有音轨" });
        None
    };
    let has_audio = engine.is_some();
    println!();

    let video_line = preview_media::video_args(&args.input, args.start);
    println!("视频命令行：ffmpeg {}", video_line.join(" "));
    println!();

    let shown = Arc::new(AtomicU64::new(0));
    let set_ms = Arc::new(Mutex::new(Vec::<f64>::new()));
    let status = Arc::new(Mutex::new(String::from("准备中")));
    let failed = Arc::new(AtomicI64::new(0));
    let finished = Arc::new(AtomicBool::new(false));

    let player = Player::new(&args, pr.framing.clone(), engine);
    player.spawn_threads();

    let framing = pr.framing.clone();
    let director = {
        let p = player.clone();
        let a = args.clone();
        let shown = shown.clone();
        let set_ms = set_ms.clone();
        let status = status.clone();
        let failed = failed.clone();
        let finished = finished.clone();
        let audio_gates = audio_gates;
        std::thread::Builder::new()
            .name("spike-director".into())
            .spawn(move || {
                // 跳转那条路要调 Reset / Start / GetCurrentPadding，都在这一层
                if has_audio && wasapi::init_com().is_err() {
                    eprintln!("流程线程 init_com 失败");
                }
                let mut gates = audio_gates;
                gates.extend(run_phases(&p, &a, &framing, &shown, &set_ms, &status));
                *status.lock().unwrap() = "收尾".into();
                p.stop_all();
                if !has_audio {
                    gates.push(gate("音画同步 / 不注静音", None, "无音轨，这条不适用"));
                }
                if a.no_window {
                    gates.push(gate("显示链路 / 送到 Slint", None, "没开窗口，这条不适用"));
                }

                println!("=== 门槛表 ===");
                let mut nfail = 0;
                for g in &gates {
                    println!("[{}] {}：{}", verdict(g), g.name, g.detail);
                    if g.pass == Some(false) {
                        nfail += 1;
                    }
                }
                println!();
                if nfail == 0 {
                    println!("全部通过（或本次不适用）");
                } else {
                    println!("{nfail} 条没过");
                }
                failed.store(nfail, Ordering::Release);
                finished.store(true, Ordering::Release);
                if has_audio {
                    wasapi::uninit_com();
                }
                let _ = slint::quit_event_loop();
            })
            .expect("起流程线程")
    };

    if args.no_window {
        // 没窗口就没有事件循环，等流程线程自己跑完
        let _ = director.join();
    } else {
        let ui = SpikeWindow::new().expect("建窗口");
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(30), {
            let weak = ui.as_weak();
            let p = player.clone();
            let shown = shown.clone();
            let set_ms = set_ms.clone();
            let status = status.clone();
            move || {
                let Some(ui) = weak.upgrade() else { return };
                let t = Instant::now();
                if let Some(f) = p.take_slot() {
                    ui.set_preview_frame(f.to_image());
                    ui.window().request_redraw();
                    set_ms
                        .lock()
                        .unwrap()
                        .push(t.elapsed().as_secs_f64() * 1000.0);
                    shown.fetch_add(1, Ordering::Relaxed);
                }
                let txt = status.lock().unwrap().clone();
                ui.set_status(txt.as_str().into());
            }
        });
        ui.show().expect("显示窗口");
        // 流程线程跑完会 quit_event_loop；窗口被用户提前关掉也在这里返回。
        // 这里不等流程线程：提前关窗口时它还在跑，等下去就是白卡几分钟。
        slint::run_event_loop().expect("跑事件循环");
        drop(director);
    }
    let _ = finished;

    wasapi::lower_timer_resolution();
    std::process::exit(failed.load(Ordering::Acquire) as i32);
}
