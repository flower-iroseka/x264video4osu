//! 视频预览：两个 ffmpeg 子进程解码，画面交给 Slint 自己画，声音走 WASAPI。
//!
//! ## 怎么放的
//!
//! 视频子进程把画面解成固定 640×360 的 `rgb24` 从 stdout 吐出来，每帧 691200 字节
//! 整读进 `SharedPixelBuffer`，转成 `slint::Image` 交给 UI；音频子进程出 PCM，
//! WASAPI 渲染到设备上，同时用 `IAudioClock` 当主时钟。
//!
//! 之前是 libmpv 加一块原生子窗口。换掉之后发布包里少一个 101 MB 的 DLL，而 ffmpeg
//! 本来就要随程序发。
//!
//! ## 时间轴
//!
//! 每帧的源时间不是从 ffmpeg 那边读来的，是用 [`Framing`] 算的：本次会话第一帧的
//! 源帧号是 `framing.first_frame_at_or_after(seek)`，第 i 帧的源时间就是
//! `framing.frame_to_secs(frame0 + i)`。这样播放和裁剪打点用的是同一个映射，
//! 而不是两套碰巧对得上的东西。
//!
//! 这条成立的前提是 `preview_media::video_args` 里那个 `-fps_mode:v passthrough`：
//! 少了它 ffmpeg 会按自己的帧率策略补帧丢帧，帧号和时间立刻对不上。
//!
//! ## 线程
//!
//! * 控制线程：收命令、开关文件和子进程、跳转。COM 和音频设备都归它。
//! * 调度线程：看时钟走到哪儿，把该显示的那一帧挑出来放进出帧口。
//! * 音频渲染线程：[`AudioEngine::render_loop`]。
//! * 每条会话自己再起两条读数线程（视频管道、PCM 管道），随会话一起进退。
//!
//! UI 线程一个都不碰，也不初始化 COM。**这一点是硬要求**：winit 建窗口时会把 UI
//! 线程按 STA 初始化（`OleInitialize`），在那之后同一个线程再要 MTA 会直接失败。
//! 所以音频设备必须在预览自己的线程上开。
//!
//! ## 发布的是哪一刻
//!
//! [`PreviewState::time_pos`] 发布的是**当前显示那一帧自己的时间**，不是时钟时间。
//! 时钟天然领先画面不到一帧；暂停时它冻在"已渲染"的位置，队列里还压着约 100 ms
//! 音频，从它算出来的位置只会更旧。打点工具要的是"我正在看的这一帧"。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use slint::Image;

use crate::platform::wasapi::{self, AudioEngine};
use crate::services::preview_media::{AudioSpec, Frame, FrameRing, FrameSecs, Session, SourceSpec};
use crate::services::probe::Framing;

/// 音量上限。滑块 0..100 正好和实际音量一一对应（界面值是百分比）。
pub const VOLUME_MAX: f64 = 100.0;

/// PCM 环留几秒的量。够吸收跳转时子进程启动的抖动，又不至于让暂停期间积压无限涨。
const PCM_SECS: usize = 4;

/// 离下一帧还差这么点时间就不再睡、改成自旋收尾。一帧 41.7 ms 里自旋 1 ms，
/// 占一个核的 2% 上下。理由见 [`sched_loop`] 里那段注释。
const SPIN_SECS: f64 = 0.001;

/// 起子进程到第一帧的等待上限
const FIRST_FRAME_LIMIT: Duration = Duration::from_millis(2000);
/// 等第一帧时每次醒来看一眼的间隔。分小段是为了被叫停时能很快退出来。
const AWAIT_STEP_MS: u64 = 5;
/// 跳转后等设备缓冲攒够的上限
const PRIME_LIMIT: Duration = Duration::from_millis(400);
/// 控制线程多久醒一次看看有没有被叫停
const CONTROL_TICK: Duration = Duration::from_millis(20);

/// 预览面板要显示的状态。UI 每 30ms 取一次。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PreviewState {
    /// 文件已经装载并且能放
    pub loaded: bool,
    pub paused: bool,
    /// 当前显示那一帧的源时间（秒）；没加载文件时为 0
    pub time_pos: f64,
    /// 总时长（秒）；未知时为 0
    pub duration: f64,
    /// 音量 0..=[`VOLUME_MAX`]
    pub volume: f64,
    pub mute: bool,
}

impl Default for PreviewState {
    fn default() -> Self {
        // 不能 derive：derive 出来的 volume 是 0（静音），状态还没读到之前界面会按
        // 这个默认值先画一帧，滑块于是先闪到最左边。
        Self {
            loaded: false,
            paused: false,
            time_pos: 0.0,
            duration: 0.0,
            volume: VOLUME_MAX,
            mute: false,
        }
    }
}

impl PreviewState {
    /// 进度条用：总时长未知时返回 0，避免除零。
    pub fn progress(&self) -> f64 {
        if self.duration > 0.0 {
            (self.time_pos / self.duration).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

/// UI 发来的打开请求。
struct OpenRequest {
    path: PathBuf,
    framing: Framing,
    duration: f64,
    has_audio: bool,
}

/// UI 发给预览线程的命令。
enum Command {
    Open(Box<OpenRequest>),
    SetPaused(bool),
    /// 播放/暂停取反，由控制线程读实时状态决定方向。
    TogglePlay,
    Seek(f64),
    /// 相对当前位置跳，单位秒，可负。
    SeekBy(f64),
    /// 正数前进、负数后退，单位是帧。逐帧步进用 ±1。
    StepFrames(i64),
    /// 音量，0..=[`VOLUME_MAX`]
    SetVolume(f64),
    /// 在当前音量上加减，由控制线程读实时值。快捷键用，理由同 [`Command::ToggleMute`]。
    StepVolume(f64),
    /// 静音取反，同样由控制线程读实时状态。
    ToggleMute,
}

/// 预览会话。持有它就代表预览开着；`Drop` 会停掉控制线程、子进程和音频设备。
pub struct Preview {
    cmd_tx: Sender<Command>,
    state: Arc<Mutex<PreviewState>>,
    /// 等 UI 取走的帧
    out: Arc<Mutex<Outbox>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// 出帧口：调度线程把该显示的帧放进来，UI 线程取走。
///
/// `notify` 是界面登记的推送回调。登记了的话，放帧时顺手叫它一声，界面立刻就
/// 同步，不用等下一个 30 ms 轮询——逐帧连点时的延迟从 0~30 ms 掉到几毫秒，
/// 播放也从轮询的 33 Hz 上限提到跟着显示器刷新率走：拿 60 fps 的素材量过，
/// 调度线程每秒推 60 帧、界面每秒取 60 帧、每秒画 60 次，其中 56~59 次画的是
/// 新帧，也就是一秒钟真的换掉五十八九张画面，而不是被砍到 33。没登记（测试里、
/// 或者窗口已经没了）就还是原来那样等 UI 来取。
#[derive(Default)]
struct Outbox {
    frame: Option<Frame>,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// 放一帧进去，登记了推送回调就叫它一声。
///
/// 回调是在放帧的那条线程上跑的，只该做「把 UI 线程叫醒」这一件事；真正的
/// 搬帧、设属性、重绘都回到 UI 线程上做，所以这里先把锁放掉再叫。
fn put_frame(out: &Mutex<Outbox>, f: Frame) {
    let cb = {
        let mut g = out.lock().unwrap();
        g.frame = Some(f);
        g.notify.clone()
    };
    if let Some(cb) = cb {
        cb();
    }
}

/// 预览能不能用：ffmpeg 在不在。缺了就是预览入口藏起来，程序其余部分照常。
pub fn is_available() -> bool {
    crate::services::ffmpeg_config::ffmpeg_path().exists()
}

impl Preview {
    /// 起一个预览会话。
    ///
    /// 内部要等控制线程把音频设备和调度线程都准备好才返回，所以是阻塞的
    /// （正常几十毫秒）。失败时返回错误描述，调用方据此把预览入口禁掉。
    ///
    /// 音频设备开不起来不算失败：没声音也照样放画面，只往 stderr 记一行。
    pub fn new() -> Result<Self, String> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let state = Arc::new(Mutex::new(PreviewState::default()));
        let out = Arc::new(Mutex::new(Outbox::default()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let state = state.clone();
            let out = out.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("preview-ctl".into())
                .spawn(move || worker(cmd_rx, ready_tx, state, out, stop))
                .map_err(|e| format!("创建预览线程失败：{e}"))?
        };

        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(Self {
                cmd_tx,
                state,
                out,
                stop,
                thread: Some(thread),
            }),
            // 控制线程已经退出了，那个 JoinHandle 不用再管；上面那个 `cmd_tx`
            // 在返回时析构，它那边的 recv 会立刻断开，不会多留一条线。
            Ok(Err(e)) => Err(e),
            Err(_) => Err("预览线程启动超时".into()),
        }
    }

    /// 当前状态快照。
    pub fn state(&self) -> PreviewState {
        *self.state.lock().unwrap()
    }

    /// 取走当前该显示的那一帧。没有新帧就返回 `None`。
    ///
    /// 调度线程已经把帧挑好了，这里只是把它包成 `slint::Image`：
    /// `SharedPixelBuffer` 是引用计数的，像素一个字节都不搬。
    ///
    /// 登记的推送回调会叫 UI 立刻来取；没登记（测试里、或者窗口已经没了）就靠
    /// UI 那个 30ms 轮询。
    pub fn take_frame(&self) -> Option<Image> {
        let f = self.out.lock().unwrap().frame.take()?;
        Some(f.to_image())
    }

    /// 登记推送回调：有新帧放进出帧口时叫一下。
    ///
    /// 回调在预览自己的线程上跑，必须很快返回，而且不能碰 `Preview` 上的东西
    /// （那条线程正锁着出帧口）。正常就一件事：让 UI 线程醒过来同步一次。
    pub fn set_frame_notify(&self, f: impl Fn() + Send + Sync + 'static) {
        self.out.lock().unwrap().notify = Some(Arc::new(f));
    }

    /// 换一个文件。
    ///
    /// `framing` 和 `duration` 由调用方给，后端自己不去探：帧↔时间的那套映射必须
    /// 和编码、裁剪打点用同一份，另探一份早晚会对不上。`has_audio` 同理，
    /// 判断有没有音轨也要动 probe，不该在这里再跑一遍。
    pub fn open(&self, path: &Path, framing: Framing, duration: f64, has_audio: bool) {
        let _ = self.cmd_tx.send(Command::Open(Box::new(OpenRequest {
            path: path.to_path_buf(),
            framing,
            duration,
            has_audio,
        })));
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.cmd_tx.send(Command::SetPaused(paused));
    }

    /// 播放/暂停取反。
    ///
    /// 不用 `set_paused(!state().paused)`：`state()` 是轮询出来的快照，长按空格时的
    /// 自动重复会连着发好几条命令，拿旧快照算就会翻错。交给控制线程读实时值。
    pub fn toggle_paused(&self) {
        let _ = self.cmd_tx.send(Command::TogglePlay);
    }

    /// 跳到绝对位置（秒）。精确到帧，用在进度条松手之后。
    pub fn seek(&self, secs: f64) {
        if secs.is_finite() && secs >= 0.0 {
            let _ = self.cmd_tx.send(Command::Seek(secs));
        }
    }

    /// 相对当前位置跳（秒，可负）。快捷键用，两端会被夹住。
    pub fn seek_by(&self, secs: f64) {
        if secs.is_finite() && secs != 0.0 {
            let _ = self.cmd_tx.send(Command::SeekBy(secs));
        }
    }

    /// 逐帧步进（±1 帧）。
    pub fn step_frames(&self, n: i64) {
        if n != 0 {
            let _ = self.cmd_tx.send(Command::StepFrames(n));
        }
    }

    /// 设音量，0..=[`VOLUME_MAX`]。超出范围的会被夹住。
    pub fn set_volume(&self, volume: f64) {
        if volume.is_finite() {
            let _ = self.cmd_tx.send(Command::SetVolume(volume));
        }
    }

    /// 在当前音量上加减 `delta`。快捷键用。
    ///
    /// 和 [`Preview::toggle_paused`] 一样交给控制线程算：`state()` 是轮询出来的
    /// 快照，连按 ↑ 时几条命令都拿同一份旧值加 5，结果只涨了一档。
    pub fn step_volume(&self, delta: f64) {
        if delta.is_finite() && delta != 0.0 {
            let _ = self.cmd_tx.send(Command::StepVolume(delta));
        }
    }

    /// 静音取反。和 [`Preview::toggle_paused`] 一样，交给控制线程读实时值，
    /// 免得连按 M 时拿旧快照翻错。
    pub fn toggle_mute(&self) {
        let _ = self.cmd_tx.send(Command::ToggleMute);
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ------------------------------------------------------------------ 时钟

/// 会话内的时间，微秒。跳转时归零，暂停时冻住。
///
/// 有音轨就用 WASAPI 的设备时钟，没有就自己数墙钟。两边对外是一个形状，
/// 调度那边不用分叉。
enum Clock {
    Audio(Arc<AudioEngine>),
    Wall {
        t_ref: Instant,
        /// 零点在 `t_ref` 上的位置
        base_ns: AtomicU64,
        /// 暂停时刻的位置
        frozen_ns: AtomicU64,
        running: AtomicBool,
    },
}

impl Clock {
    fn wall() -> Clock {
        Clock::Wall {
            t_ref: Instant::now(),
            base_ns: AtomicU64::new(0),
            frozen_ns: AtomicU64::new(0),
            running: AtomicBool::new(false),
        }
    }

    fn local_us(&self) -> i64 {
        match self {
            Clock::Audio(e) => e.local_us(),
            Clock::Wall {
                t_ref,
                base_ns,
                frozen_ns,
                running,
            } => {
                if !running.load(Ordering::Acquire) {
                    return (frozen_ns.load(Ordering::Acquire) / 1000) as i64;
                }
                let now = t_ref.elapsed().as_nanos() as u64;
                (now.saturating_sub(base_ns.load(Ordering::Acquire)) / 1000) as i64
            }
        }
    }

    /// 暂停。设备时钟是把当前值冻住再停流——顺序不能反，停完就问不出位置了。
    fn pause(&self) {
        match self {
            Clock::Audio(e) => e.pause(),
            Clock::Wall {
                t_ref,
                base_ns,
                frozen_ns,
                running,
            } => {
                if !running.load(Ordering::Acquire) {
                    return;
                }
                let now = t_ref.elapsed().as_nanos() as u64;
                frozen_ns.store(
                    now.saturating_sub(base_ns.load(Ordering::Acquire)),
                    Ordering::Release,
                );
                running.store(false, Ordering::Release);
            }
        }
    }

    /// 把本次会话的时间归零，顺便停下。接着播要再 [`Clock::resume`]。
    fn reset(&self) {
        match self {
            Clock::Audio(e) => e.reset(),
            Clock::Wall {
                t_ref,
                base_ns,
                frozen_ns,
                running,
            } => {
                running.store(false, Ordering::Release);
                frozen_ns.store(0, Ordering::Release);
                base_ns.store(t_ref.elapsed().as_nanos() as u64, Ordering::Release);
            }
        }
    }

    /// 从暂停或者归零状态接着走。
    fn resume(&self) {
        match self {
            Clock::Audio(e) => e.start(),
            Clock::Wall {
                t_ref,
                base_ns,
                frozen_ns,
                running,
            } => {
                if running.load(Ordering::Acquire) {
                    return;
                }
                let frozen = frozen_ns.load(Ordering::Acquire);
                let now = t_ref.elapsed().as_nanos() as u64;
                base_ns.store(now.saturating_sub(frozen), Ordering::Release);
                running.store(true, Ordering::Release);
            }
        }
    }
}

// ------------------------------------------------------------------ 调度

/// 控制线程和调度线程共享的那部分：现在放哪个环、从哪儿起、走不走设备时钟。
struct Live {
    ring: Option<Arc<FrameRing>>,
    /// 当前文件的帧号→时间映射
    map: Option<FrameSecs>,
    /// 本次会话起点在源文件里的秒数
    seek: f64,
    /// 本次会话是不是真的在走设备时钟
    audio: bool,
}

struct Sched {
    /// 有音轨时用的设备时钟。没开出设备就没有这个。
    device: Option<Arc<Clock>>,
    /// 没音轨时自己数的墙钟
    wall: Arc<Clock>,
    live: Arc<Mutex<Live>>,
    out: Arc<Mutex<Outbox>>,
    state: Arc<Mutex<PreviewState>>,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

/// 出帧调度：时钟走到某帧的时间点，就把它丢进出帧口。
///
/// 单独起一条线程、按帧的点叫醒，而不是让 UI 那个 30ms 定时器顺手挑一帧：
/// 后者挑的是"到现在为止最新的那帧"，误差就是一个 tick 的量级（p50 十几毫秒），
/// 音画同步根本够不着。
fn sched_loop(s: Arc<Sched>) {
    // 要读 IAudioClock 的位置，就得自己初始化一次 COM
    if s.device.is_some() && wasapi::init_com().is_err() {
        eprintln!("预览调度线程初始化 COM 失败，画面不会动");
        return;
    }

    loop {
        if s.stop.load(Ordering::Acquire) {
            wasapi::uninit_com();
            return;
        }
        if s.paused.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }

        let g = s.live.lock().unwrap();
        let (Some(ring), Some(map)) = (g.ring.clone(), g.map.clone()) else {
            drop(g);
            std::thread::sleep(Duration::from_millis(2));
            continue;
        };
        let seek = g.seek;
        let audio = g.audio;
        // 读哪个时钟得先看这个文件走不走设备：没音轨的会话上设备是停着的，
        // 拿它的位置来算，位置永远停在原地，画面就再也不动了。
        let raw_local = match (&s.device, audio) {
            (Some(d), true) => d.local_us(),
            _ => s.wall.local_us(),
        } as f64
            / 1e6;

        let next_local = map(ring.cursor() + 1) - seek;
        // 墙钟不能让位置跑到下一帧前头去：解码卡住的时候它会自己往前爬，
        // 一放开就把内容整段跳过去了。设备时钟不用管，它的位置本来就跟着音频走。
        let local = if audio {
            raw_local
        } else {
            raw_local.min(next_local)
        };

        if next_local > local {
            let d = next_local - local;
            drop(g);
            // 一次睡掉大头，最后这一小段自己转。
            //
            // 不能"每次只睡 d 的一半、循环着逼近"：本机实测 `sleep` 每次要多花约
            // 0.5 ms（`sleep(1ms)` 实际 1.53 ms），一帧拆成十来次小睡就把 41.7 ms
            // 累积成 50 ms 以上，于是每隔几帧整个跳过一帧。跳过的那帧不会体现成
            // 排期误差（下一帧就在那儿，误差照样很小），只会体现成帧率掉下去。
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
                let secs = f.secs;
                drop(g);
                // 发布的是这一帧自己的时间，不是时钟时间。
                //
                // 顺序不能反：推送回调一响，UI 线程马上就照着 state 去更新帧号，
                // 时间先写进去，界面才不会拿到上一帧的读数。
                s.state.lock().unwrap().time_pos = secs;
                put_frame(&s.out, f);
            }
            None => {
                let done = ring.is_eof() && ring.back_index().is_none_or(|b| b <= ring.cursor());
                drop(g);
                if done {
                    // 后面没有了，最后那帧留在屏上
                    std::thread::sleep(Duration::from_millis(20));
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
}

// ------------------------------------------------------------------ 控制

/// 控制线程的全部状态。
struct Ctl {
    ffmpeg: String,
    engine: Option<Arc<AudioEngine>>,
    /// 设备时钟。`engine` 没开出来（或者没有音频设备）时是 `None`。
    device: Option<Arc<Clock>>,
    /// 自己数的墙钟。没音轨的文件用它当主时钟。
    wall: Arc<Clock>,
    live: Arc<Mutex<Live>>,
    out: Arc<Mutex<Outbox>>,
    state: Arc<Mutex<PreviewState>>,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    render: Option<JoinHandle<()>>,
    sched: Option<JoinHandle<()>>,
    sess: Option<Session>,

    /// 当前文件
    path: PathBuf,
    framing: Framing,
    map: Option<FrameSecs>,
    duration: f64,
    has_audio: bool,
}

/// 预览线程：开音频设备、起调度和渲染线程、然后跑命令循环。
fn worker(
    cmd_rx: Receiver<Command>,
    ready_tx: Sender<Result<(), String>>,
    state: Arc<Mutex<PreviewState>>,
    out: Arc<Mutex<Outbox>>,
    stop: Arc<AtomicBool>,
) {
    if !is_available() {
        let _ = ready_tx.send(Err("找不到 tools/ffmpeg.exe".into()));
        return;
    }

    // 默认 15.6 ms 的定时器精度会把渲染线程的等待粒度整个吃掉
    wasapi::raise_timer_resolution();

    // 设备只在这一条线程上开、整个会话只开一次。跳转只换锚点，不重建设备：
    // 重建除了慢，还会引入新的音画不同步。
    //
    // 就在控制线程上开，不要挪到别处——UI 线程已经被 winit 占成 STA 了，
    // 那儿 `CoInitializeEx(MTA)` 会直接失败。
    let engine = match wasapi::init_com().and_then(|_| AudioEngine::open()) {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("预览没有声音：{e}");
            None
        }
    };

    // 两个时钟都建好，具体用哪个按当前文件有没有音轨挑（见 `Ctl::clock`）
    let device = engine.as_ref().map(|e| Arc::new(Clock::Audio(e.clone())));
    let wall = Arc::new(Clock::wall());
    let live = Arc::new(Mutex::new(Live {
        ring: None,
        map: None,
        seek: 0.0,
        audio: false,
    }));
    let paused = Arc::new(AtomicBool::new(false));

    let render = engine.as_ref().and_then(|e| {
        let e = e.clone();
        spawn_thread("preview-audio", move || {
            // 每个碰音频接口的线程都自己初始化一次 COM，都按 MTA
            if let Err(err) = wasapi::init_com() {
                eprintln!("预览音频线程初始化 COM 失败：{err}");
                return;
            }
            AudioEngine::render_loop(&e);
            wasapi::uninit_com();
        })
    });

    let sched = spawn_thread("preview-sched", {
        let s = Arc::new(Sched {
            device: device.clone(),
            wall: wall.clone(),
            live: live.clone(),
            out: out.clone(),
            state: state.clone(),
            paused: paused.clone(),
            stop: stop.clone(),
        });
        move || sched_loop(s)
    });
    let Some(sched) = sched else {
        let _ = ready_tx.send(Err("创建预览调度线程失败".into()));
        // 就走到这儿的话 `ctl.shutdown()` 永远不会跑，没人收音频那边：
        // 设备一直开着、渲染线程一直转，而且上面那次 `init_com` 也没人配对上。
        // 按 `shutdown` 里同样的顺序自己收一遍。
        if let Some(e) = &engine {
            e.shutdown();
        }
        if let Some(h) = render {
            let _ = h.join();
        }
        wasapi::lower_timer_resolution();
        wasapi::uninit_com();
        return;
    };

    let mut ctl = Ctl {
        ffmpeg: crate::services::ffmpeg_config::ffmpeg_path()
            .to_string_lossy()
            .into_owned(),
        engine,
        device,
        wall,
        live,
        out,
        state,
        paused,
        stop,
        render,
        sched: Some(sched),
        sess: None,
        path: PathBuf::new(),
        framing: Framing::Unknown,
        map: None,
        duration: 0.0,
        has_audio: false,
    };

    let _ = ready_tx.send(Ok(()));

    loop {
        if ctl.stop.load(Ordering::Acquire) {
            break;
        }
        match cmd_rx.recv_timeout(CONTROL_TICK) {
            Ok(cmd) => match cmd {
                Command::Seek(t) => ctl.seek_and_drain(&cmd_rx, t),
                Command::SeekBy(d) => {
                    let now = ctl.now_secs();
                    ctl.seek_and_drain(&cmd_rx, now + d)
                }
                other => ctl.handle(other),
            },
            Err(RecvTimeoutError::Timeout) => {}
            // 发送端没了，也就是 `Preview` 已经析构。再转下去就是空转，收工。
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    ctl.shutdown();
}

fn spawn_thread(name: &str, f: impl FnOnce() + Send + 'static) -> Option<JoinHandle<()>> {
    match std::thread::Builder::new().name(name.into()).spawn(f) {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("创建线程 {name} 失败：{e}");
            None
        }
    }
}

/// 把 `t` 夹进文件范围。时长未知时只保证非负。
///
/// 时长未知那一支要单独写：`secs.clamp(0.0, secs)` 在 `secs` 为负时两个端点
/// 就反了，直接 panic。往回跳正好会算出负的时间（`time_pos + (-5.0)`），
/// 而"帧率读得到、时长读不到"的文件是有的，这条路上控制线程一死预览就永远卡住。
fn clamp_seek(duration: f64, secs: f64) -> f64 {
    if !secs.is_finite() {
        return 0.0;
    }
    if duration > 0.0 {
        secs.clamp(0.0, duration)
    } else {
        secs.max(0.0)
    }
}

impl Ctl {
    fn use_audio(&self) -> bool {
        self.has_audio && self.engine.is_some()
    }

    /// 当前文件该走哪个时钟。有音轨就走设备时钟，没有就自己数墙钟。
    ///
    /// 用 `engine.is_some()` 挑是不对的：设备开着不代表这个文件有音轨。没音轨的
    /// 会话里设备是停着的，拿它的位置当主时钟，位置不走，播放就整个停在那儿。
    fn clock(&self) -> &Clock {
        match (self.use_audio(), &self.device) {
            (true, Some(d)) => d,
            _ => &self.wall,
        }
    }

    /// 发布当前显示那一帧的时间。
    fn publish(&self, secs: f64) {
        self.state.lock().unwrap().time_pos = secs;
    }

    fn now_secs(&self) -> f64 {
        self.state.lock().unwrap().time_pos
    }

    /// 收到跳转时的入口：先把队列里已经排上的跳转一并收掉，只真跳一次。
    ///
    /// 一次跳转要停掉旧的、起新的两个解码子进程，实测 150–270 ms 才出第一帧。
    /// 方向键按住不放会按系统重复率连发（30/s 上下），一条条照做的话队列越积越长，
    /// 松手之后还得追好几秒。所以真去跳之前先看一眼队列里还排着什么。
    ///
    /// 相对跳转是**加起来**、不是只留最后一条：按住方向键本来就是"一直往前挪"，
    /// 只留最后一条会变成按多久也只挪一次。
    fn seek_and_drain(&mut self, rx: &Receiver<Command>, target: f64) {
        let mut target = target;
        let mut others = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Command::Seek(t) => target = t,
                Command::SeekBy(d) => target += d,
                // 其它命令一条都不能丢，也不能插到跳转前面去。先记下来，
                // 跳完再按原来的先后顺序执行。
                other => others.push(other),
            }
        }
        self.handle(Command::Seek(target));
        for c in others {
            self.handle(c);
        }
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Open(req) => self.open(*req),
            Command::SetPaused(p) => self.set_paused(p),
            // 读实时值再取反，不能拿 UI 那份轮询快照
            Command::TogglePlay => self.set_paused(!self.paused.load(Ordering::Acquire)),
            Command::Seek(t) => self.seek_abs(t),
            Command::SeekBy(d) => {
                let now = self.state.lock().unwrap().time_pos;
                self.seek_abs(now + d);
            }
            Command::StepFrames(n) => self.step(n),
            Command::SetVolume(v) => self.set_volume(v),
            Command::StepVolume(d) => {
                let v = self.state.lock().unwrap().volume + d;
                self.set_volume(v);
            }
            Command::ToggleMute => {
                let m = self.state.lock().unwrap().mute;
                self.set_mute(!m);
            }
        }
    }

    fn open(&mut self, req: OpenRequest) {
        self.path = req.path;
        self.framing = req.framing;
        self.duration = req.duration;
        self.has_audio = req.has_audio;
        let f = self.framing.clone();
        self.map = Some(Arc::new(move |i: i64| f.frame_to_secs(i)));

        // 帧率拿不到就没法给帧编号，也就没有时间轴。按错的帧率硬放只会让帧号和
        // 画面对不上，不如当它加载不了。
        let usable = self.framing.is_valid();
        {
            let mut s = self.state.lock().unwrap();
            s.loaded = usable;
            s.duration = req.duration;
            s.time_pos = 0.0;
        }
        if usable {
            self.start_at(0.0);
        } else {
            self.stop_session();
        }
    }

    fn set_paused(&mut self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
        self.state.lock().unwrap().paused = paused;
        if paused {
            self.clock().pause();
        } else {
            // 设备不重建、音频不重灌：缓冲里的东西还在，直接接着走
            self.clock().resume();
        }
    }

    fn set_volume(&mut self, volume: f64) {
        let volume = volume.clamp(0.0, VOLUME_MAX);
        let mute = {
            let mut s = self.state.lock().unwrap();
            s.volume = volume;
            s.mute
        };
        self.push_volume(volume, mute);
    }

    fn set_mute(&mut self, mute: bool) {
        let volume = {
            let mut s = self.state.lock().unwrap();
            s.mute = mute;
            s.volume
        };
        self.push_volume(volume, mute);
    }

    fn push_volume(&self, volume: f64, mute: bool) {
        if let Some(e) = &self.engine {
            e.set_volume(if mute { 0.0 } else { volume as f32 });
        }
    }

    fn seek_abs(&mut self, secs: f64) {
        if self.map.is_none() {
            return;
        }
        self.start_at(secs);
    }

    /// 跳到 `t` 秒：停掉当前会话，起新的，装环、换 PCM 源、起时钟，等第一帧。
    fn start_at(&mut self, t: f64) {
        let Some(map) = self.map.clone() else { return };
        let t = clamp_seek(self.duration, t);
        let frame0 = self.framing.first_frame_at_or_after(t);

        // 先摘环：调度线程立刻停手，免得它拿着旧环又吐一帧出去
        {
            let mut g = self.live.lock().unwrap();
            g.ring = None;
            g.map = Some(map.clone());
            g.seek = t;
            g.audio = self.use_audio();
        }
        self.out.lock().unwrap().frame = None;
        // 先按"马上要显示的那一帧"的时间更新一次，界面立刻跟上；第一帧真到了
        // 还会再写一遍，两者本该一致。
        self.publish(map(frame0));

        // 设备停流，把残留的旧 PCM 冲掉。Reset 只在停止状态合法，顺序不能反。
        //
        // 两个时钟都归零。不只是"顺手"：从有声文件切到没声文件时，设备可能还开着，
        // 这一下才把它停掉；反过来的话墙钟也只是白重设一次。
        if let Some(d) = &self.device {
            d.reset();
        }
        self.wall.reset();

        // 旧会话在这里丢掉：子进程杀掉、两条读数线程 join 掉
        self.stop_session();

        self.start_session(t, frame0);
    }

    fn audio_spec(&self) -> Option<AudioSpec> {
        let e = self.engine.as_ref()?;
        let f = e.format();
        Some(AudioSpec {
            rate: f.rate,
            channels: f.channels,
            sample: f.ffmpeg_sample().to_string(),
        })
    }

    fn start_session(&mut self, t: f64, frame0: i64) {
        let spec = SourceSpec {
            ffmpeg: self.ffmpeg.clone(),
            input: self.path.to_string_lossy().into_owned(),
            seek: t,
            frame0,
            frame_secs: self.map.clone().expect("open 里已经装好"),
            audio: if self.use_audio() { self.audio_spec() } else { None },
        };
        let pcm_limit = spec
            .audio
            .as_ref()
            .map_or(0, |a| a.rate as usize * a.block_align() * PCM_SECS);

        let sess = match Session::start(spec, pcm_limit) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("预览起解码进程失败：{e}");
                self.state.lock().unwrap().loaded = false;
                return;
            }
        };

        let ring = sess.frames().clone();
        // 无条件设一次：没音轨的文件也要把上一次的源清掉。只在两边都有的分支里设的话，
        // 从有声文件切到没声文件时设备上还挂着旧会话的 PcmFill，那个闭包一直钉着旧环
        // （PCM_SECS 那一份，几百 KB），而且下次真出声时喂的还是上一个文件的数据。
        if let Some(engine) = self.engine.clone() {
            match sess.pcm().cloned() {
                Some(pcm) => {
                    let fill: wasapi::PcmFill =
                        Arc::new(move |dst, need| unsafe { pcm.fill(dst, need) });
                    engine.set_source(Some(fill));
                }
                // 没音轨：清掉上一次的源，设备照走，只是出静音
                None => engine.set_source(None),
            }
        }
        self.sess = Some(sess);
        self.live.lock().unwrap().ring = Some(ring.clone());
        self.state.lock().unwrap().loaded = true;

        // 先攒够一个设备缓冲再启动。不这么做的话，渲染引擎在等 ffmpeg 出声音的
        // 那段时间里播的是零、时钟却在走，媒体时间会比第一个真实采样早一百多毫秒。
        // 暂停状态下只攒不启动，等用户按播放。
        let playing = !self.paused.load(Ordering::Acquire);
        if let (Some(e), true) = (&self.engine, self.use_audio()) {
            if playing {
                let _ = e.prime_and_start(PRIME_LIMIT);
            } else {
                e.prefill();
            }
        }

        // 等第一帧。跳转之后环是空的，先摆一帧上去，画面不会停在旧内容上。
        // 真等不到（比如跳到了末尾）就退回环里最早的那帧，总比留一片空白强。
        //
        // 这里分小段轮询而不是一次 `wait_for_index` 等到底：一次等到头的话，
        // 用户在这 2 秒里关窗口、或者又点了别的文件，控制线程都收不到，
        // 关窗口会明显卡一下。
        let first = self.await_frame(&ring, frame0);
        if let Some(f) = first {
            ring.set_cursor(f.index);
            let secs = f.secs;
            self.publish(secs);
            put_frame(&self.out, f);
        }

        // 没音轨时墙钟等第一帧落位再起步，否则开头那段解码时间会白算进去
        if !self.use_audio() && playing {
            self.wall.resume();
        }
    }

    /// 等 `index` 那一帧出现，最多等 [`FIRST_FRAME_LIMIT`]；被叫停就提前退出。
    ///
    /// 拿不到就退回环里已有的最后一帧，屏上总得有点东西可看；一个视频帧都没有
    /// （比如选了个没有视频轨的文件）才返回 `None`。
    fn await_frame(&self, ring: &FrameRing, index: i64) -> Option<Frame> {
        let deadline = Instant::now() + FIRST_FRAME_LIMIT;
        loop {
            if let Some(f) = ring.get(index) {
                return Some(f);
            }
            // 解码到底了就不用再等：index 要么已经解出来了，要么超出了文件末尾
            if ring.is_eof() {
                return match ring.back_index() {
                    Some(b) if b >= index => ring.get(index),
                    _ => ring.latest(),
                };
            }
            if self.stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                return ring.latest();
            }
            std::thread::sleep(Duration::from_millis(AWAIT_STEP_MS));
        }
    }

    /// 停掉当前会话并把环摘掉。可以重复调。
    fn stop_session(&mut self) {
        self.live.lock().unwrap().ring = None;
        drop(self.sess.take());
    }

    /// 逐帧走缓冲。环里没有那一帧就不动（比如已经走到本次会话的头或是尾）。
    fn step(&mut self, n: i64) {
        let g = self.live.lock().unwrap();
        let Some(ring) = g.ring.clone() else { return };
        let sign = n.signum();
        let mut cur = ring.cursor();
        let mut last = None;
        for _ in 0..n.abs() {
            let want = self.framing.clamp_frame(cur + sign);
            let Some(f) = ring.get(want) else { break };
            ring.set_cursor(f.index);
            cur = f.index;
            last = Some(f);
        }
        let Some(f) = last else { return };
        let secs = f.secs;
        drop(g);
        self.publish(secs);
        put_frame(&self.out, f);
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.sched.take() {
            let _ = h.join();
        }
        if let Some(e) = &self.engine {
            e.shutdown();
        }
        if let Some(h) = self.render.take() {
            let _ = h.join();
        }
        // 子进程最后收：读数线程都停了，不会再有人往环里写
        self.stop_session();
        wasapi::lower_timer_resolution();
        wasapi::uninit_com();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_guards_unknown_duration() {
        let s = PreviewState {
            loaded: true,
            paused: false,
            time_pos: 5.0,
            duration: 0.0,
            ..Default::default()
        };
        assert_eq!(s.progress(), 0.0);

        let s = PreviewState {
            time_pos: 5.0,
            duration: 10.0,
            ..Default::default()
        };
        assert!((s.progress() - 0.5).abs() < 1e-9);

        // 越界要夹住：帧号反算回来的时间偶尔会略微超过时长
        let s = PreviewState {
            time_pos: 11.0,
            duration: 10.0,
            ..Default::default()
        };
        assert_eq!(s.progress(), 1.0);
    }

    #[test]
    fn volume_defaults_to_full_not_zero() {
        // 界面在读到真实状态之前会先按默认值画一帧，用 0 的话滑块会先闪到最左边
        let s = PreviewState::default();
        assert_eq!(s.volume, VOLUME_MAX);
        assert!(!s.mute);
        assert!(!s.loaded);
    }

    #[test]
    fn clamp_seek_keeps_negative_input_in_range_when_duration_is_unknown() {
        // 时长读不到的文件（Duration: N/A）时长是 0。往回跳会算出负的时间，
        // 这里以前是 `secs.clamp(0.0, secs)`，两端反了，直接 panic 把控制线程打死。
        assert_eq!(clamp_seek(0.0, -5.0), 0.0);
        assert_eq!(clamp_seek(0.0, -0.001), 0.0);
        assert_eq!(clamp_seek(-1.0, -5.0), 0.0);
        assert_eq!(clamp_seek(0.0, 7.5), 7.5);
    }

    #[test]
    fn clamp_seek_fits_into_a_known_duration() {
        assert_eq!(clamp_seek(10.0, 3.0), 3.0);
        assert_eq!(clamp_seek(10.0, -3.0), 0.0);
        assert_eq!(clamp_seek(10.0, 99.0), 10.0);
    }

    #[test]
    fn clamp_seek_rejects_non_finite() {
        assert_eq!(clamp_seek(10.0, f64::NAN), 0.0);
        assert_eq!(clamp_seek(10.0, f64::INFINITY), 0.0);
        assert_eq!(clamp_seek(10.0, f64::NEG_INFINITY), 0.0);
    }

    fn sleep_ms(ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }

    #[test]
    fn wall_clock_starts_at_zero_only_after_resume() {
        let c = Clock::wall();
        // 刚建出来是停着的，位置就是 0
        assert_eq!(c.local_us(), 0);

        c.reset();
        assert_eq!(c.local_us(), 0, "reset 之后应该是停着的");

        c.resume();
        sleep_ms(20);
        let a = c.local_us();
        assert!((10_000..200_000).contains(&a), "走了 {a} µs，不像 20 ms");
    }

    #[test]
    fn wall_clock_freezes_while_paused_and_keeps_where_it_stopped() {
        let c = Clock::wall();
        c.reset();
        c.resume();
        sleep_ms(20);
        c.pause();
        let frozen = c.local_us();
        assert!(frozen > 0);

        sleep_ms(30);
        assert_eq!(c.local_us(), frozen, "暂停期间不该再往前爬");

        c.resume();
        sleep_ms(20);
        let after = c.local_us();
        assert!(
            after > frozen,
            "恢复之后应该接着走：{frozen} → {after}"
        );
        // 暂停那 30 ms 不能算进去
        assert!(after < frozen + 200_000, "停的那段被算进去了：{after}");
    }

    #[test]
    fn wall_clock_reset_zeroes_without_resuming() {
        let c = Clock::wall();
        c.reset();
        c.resume();
        sleep_ms(20);
        assert!(c.local_us() > 0);

        c.reset();
        assert_eq!(c.local_us(), 0);
        sleep_ms(20);
        assert_eq!(c.local_us(), 0, "reset 之后没 resume 就不该自己走");
    }

    #[test]
    fn ffmpeg_is_what_makes_preview_available() {
        // 以前这里看的是 libmpv-2.dll，现在是 ffmpeg.exe
        let p = crate::services::ffmpeg_config::ffmpeg_path();
        assert!(p.ends_with("ffmpeg.exe"), "路径不对: {}", p.display());
        assert_eq!(is_available(), p.exists());
    }
}
