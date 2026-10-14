//! 预览用的两个 ffmpeg 子进程，以及配套的两条管道缓冲。
//!
//! 视频子进程把画面解码成固定尺寸的 `rgb24` 从 stdout 吐出来，音频子进程把声音
//! 转成设备能直接吃的 PCM 吐出来。这个模块负责拼命令行、起进程、按整帧读、
//! 用环形缓冲把解码和播放解耦，以及跳转时把旧的会话整个换掉。
//!
//! 它**不引用 `crate::` 里的任何东西**：ffmpeg 路径、帧号→时间的映射、音频格式
//! 都是参数传进来的。这样 `examples/preview_ffmpeg_spike.rs` 能用 `#[path]`
//! include 同一个源文件，那个验证程序跑的就是产品要用的那份代码。
//!
//! ## 两个必须记住的点
//!
//! * 视频命令行里的 `-fps_mode:v passthrough` 不能省。少了它 ffmpeg 会按自己的
//!   帧率策略补帧或者丢帧，那么"第 i 帧对应什么时间"整个就散架了。
//! * 时间轴不是从 ffmpeg 那边读来的，是调用方用 [`FrameSecs`] 算的。这样播放的
//!   时间轴和裁剪打点用的是同一个映射，不是两套碰巧对上的东西。

// 产品只用到其中一部分：统计量、会话代数这些是给
// examples/preview_ffmpeg_spike.rs 看数据的，这里先压着别报未使用。
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{self, Read};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use slint::{Image, Rgb8Pixel, SharedPixelBuffer};

/// 起子进程时不弹控制台窗口
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 预览画布。固定尺寸是必须的：管道里的 rawvideo 没有逐帧头，尺寸一变就没法按
/// 固定长度切帧。宽高比交给 Slint 那边的 `image-fit: contain` 兜。
pub const FRAME_W: usize = 640;
pub const FRAME_H: usize = 360;
pub const FRAME_BYTES: usize = FRAME_W * FRAME_H * 3;

/// 解码帧环最多留多少帧（96 × 691200 B ≈ 63 MiB）
pub const RING_FRAMES: usize = 96;
/// 其中显示位置往前保留多少帧。往回逐帧、往回拖动的范围就是它。
pub const RING_HISTORY: i64 = 48;

/// 绝对帧号 → 源时间（秒）。产品里由 `probe::Framing` 提供。
pub type FrameSecs = Arc<dyn Fn(i64) -> f64 + Send + Sync>;

/// 秒转成命令行参数。六位小数够到微秒，`-ss` 本身也认小数秒。
pub fn fmt_secs(secs: f64) -> String {
    format!("{:.6}", secs.max(0.0))
}

// ------------------------------------------------------------------ 命令行

/// 视频解码命令行。
///
/// `-ss` 放在 `-i` 前面（输入定位）：实测出第一帧 266–309 ms，帧号也准确——
/// `-ss 30` 的第一帧和 `select=eq(n\,720)` 选出来的一模一样。
pub fn video_args(input: &str, seek: f64) -> Vec<String> {
    let vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,\
         pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,format=rgb24",
        w = FRAME_W,
        h = FRAME_H
    );
    [
        "-hide_banner",
        "-nostdin",
        "-nostats",
        "-loglevel",
        "error",
        "-ss",
        &fmt_secs(seek),
        "-i",
        input,
        "-map",
        "0:v:0",
        "-an",
        "-sn",
        "-dn",
        "-vf",
        &vf,
        "-fps_mode:v",
        "passthrough",
        "-pix_fmt",
        "rgb24",
        "-f",
        "rawvideo",
        "pipe:1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// 设备协商出来的 PCM 格式。`sample` 是 `aformat` 滤镜认的那个名字。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AudioSpec {
    pub rate: u32,
    pub channels: u16,
    pub sample: String,
}

impl AudioSpec {
    fn codec(&self) -> &'static str {
        if self.sample == "f32" {
            "pcm_f32le"
        } else {
            "pcm_s16le"
        }
    }

    fn muxer(&self) -> &'static str {
        if self.sample == "f32" {
            "f32le"
        } else {
            "s16le"
        }
    }

    fn layout(&self) -> &'static str {
        match self.channels {
            1 => "mono",
            2 => "stereo",
            6 => "5.1",
            8 => "7.1",
            _ => "stereo",
        }
    }

    pub fn block_align(&self) -> usize {
        let bits = if self.sample == "f32" { 4 } else { 2 };
        bits * self.channels as usize
    }
}

/// 音频解码命令行。
///
/// `aresample=...:first_pts=0` 是为了让样本计数从本次会话时间轴的起点开始：
/// 源文件的声音比画面晚起时，前面那段是等长的静音，而不是把声音整体前移。
/// PCM 流里没有时间戳，主时钟由 WASAPI 提供，所以不需要 `asetpts`。
pub fn audio_args(input: &str, seek: f64, spec: &AudioSpec) -> Vec<String> {
    let af = format!(
        "aresample={r}:async=1:first_pts=0,\
         aformat=sample_fmts={s}:sample_rates={r}:channel_layouts={l}",
        r = spec.rate,
        s = spec.sample,
        l = spec.layout()
    );
    [
        "-hide_banner",
        "-nostdin",
        "-nostats",
        "-loglevel",
        "warning",
        "-ss",
        &fmt_secs(seek),
        "-i",
        input,
        "-map",
        "0:a:0",
        "-vn",
        "-sn",
        "-dn",
        "-af",
        &af,
        "-ar",
        &spec.rate.to_string(),
        "-ac",
        &spec.channels.to_string(),
        "-c:a",
        spec.codec(),
        "-f",
        spec.muxer(),
        "pipe:1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn spawn(ffmpeg: &str, args: &[String]) -> io::Result<Child> {
    Command::new(ffmpeg)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

// ------------------------------------------------------------------ 解码帧环

/// 一帧解码好的画面，外加它在**源文件**里的位置。
#[derive(Clone)]
pub struct Frame {
    /// 源绝对帧号 = 本次会话的第一帧帧号 + 解码顺序
    pub index: i64,
    /// 源绝对时间（秒），由 [`FrameSecs`] 算出来
    pub secs: f64,
    pub pixels: SharedPixelBuffer<Rgb8Pixel>,
}

impl Frame {
    /// 交给 Slint。`SharedPixelBuffer` 是引用计数的，克隆它只是加一次计数，
    /// 像素数据不搬。
    pub fn to_image(&self) -> Image {
        Image::from_rgb8(self.pixels.clone())
    }
}

struct RingInner {
    frames: VecDeque<Frame>,
    /// 当前正在显示的那一帧的源帧号；-1 表示还没显示过
    cursor: i64,
    eof: bool,
    decoded: u64,
}

/// 解码帧的环形缓冲。
///
/// 背压靠它：环满了读线程就停在 condvar 上，于是没人读子进程的 stdout，
/// ffmpeg 写管道阻塞，解码跟着停下。内存上限就是环本身，不随文件长度涨。
pub struct FrameRing {
    inner: Mutex<RingInner>,
    cv_space: Condvar,
    cv_data: Condvar,
    cap: usize,
    frame0: i64,
    aborted: AtomicBool,
}

impl FrameRing {
    pub fn new(cap: usize, frame0: i64) -> Arc<FrameRing> {
        Arc::new(FrameRing {
            inner: Mutex::new(RingInner {
                frames: VecDeque::new(),
                // 还没显示过：停在第一帧前面。不能拿 -1 当"没显示过"的哨兵——
                // 从文件开头播的时候 frame0 就是 0，哨兵会和真实位置撞上。
                cursor: frame0 - 1,
                eof: false,
                decoded: 0,
            }),
            cv_space: Condvar::new(),
            cv_data: Condvar::new(),
            cap,
            frame0,
            aborted: AtomicBool::new(false),
        })
    }

    fn full(&self, g: &RingInner) -> bool {
        g.frames.len() >= self.cap
    }

    /// 把显示位置后面太远的旧帧扔掉——它们既轮不到显示，也不在往回逐帧够得着的
    /// 范围里。
    ///
    /// 这件事必须做，光靠 `full()` 顶住读线程是不行的：读线程一停，ffmpeg 写管道
    /// 阻塞、解码跟着停，于是再也不会有新帧进来，游标走到环尾就死在那儿。淘汰把
    /// 位置腾出来，读线程才醒得过来。
    fn evict(&self, g: &mut RingInner) {
        let keep_from = g.cursor.saturating_sub(RING_HISTORY);
        while g.frames.front().is_some_and(|f| f.index < keep_from) {
            g.frames.pop_front();
        }
    }

    /// 读线程往里放一帧。环满了就阻塞。返回 false 表示被 abort 了，该收工。
    fn push(&self, f: Frame) -> bool {
        let mut g = self.inner.lock().unwrap();
        loop {
            self.evict(&mut g);
            if !self.full(&g) {
                break;
            }
            if self.aborted.load(Ordering::SeqCst) {
                return false;
            }
            let (ng, _) = self
                .cv_space
                .wait_timeout(g, Duration::from_millis(50))
                .unwrap();
            g = ng;
        }
        if self.aborted.load(Ordering::SeqCst) {
            return false;
        }
        g.decoded += 1;
        g.frames.push_back(f);
        drop(g);
        self.cv_data.notify_all();
        true
    }

    pub fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        self.cv_space.notify_all();
        self.cv_data.notify_all();
    }

    fn finish_eof(&self) {
        self.inner.lock().unwrap().eof = true;
        self.cv_data.notify_all();
    }

    pub fn is_eof(&self) -> bool {
        self.inner.lock().unwrap().eof
    }

    pub fn decoded(&self) -> u64 {
        self.inner.lock().unwrap().decoded
    }

    pub fn backlog(&self) -> usize {
        self.inner.lock().unwrap().frames.len()
    }

    pub fn frame0(&self) -> i64 {
        self.frame0
    }

    /// 告诉环现在显示到哪儿了。淘汰和背压都按它算，所以要跟着显示位置更新。
    ///
    /// 跳转和逐帧用这个（游标要跳到任意位置）；正常播放走 [`Self::take_due`]。
    pub fn set_cursor(&self, index: i64) {
        let mut g = self.inner.lock().unwrap();
        g.cursor = index;
        self.evict(&mut g);
        drop(g);
        self.cv_space.notify_all();
    }

    pub fn cursor(&self) -> i64 {
        self.inner.lock().unwrap().cursor
    }

    pub fn front_index(&self) -> Option<i64> {
        self.inner.lock().unwrap().frames.front().map(|f| f.index)
    }

    pub fn back_index(&self) -> Option<i64> {
        self.inner.lock().unwrap().frames.back().map(|f| f.index)
    }

    pub fn get(&self, index: i64) -> Option<Frame> {
        let g = self.inner.lock().unwrap();
        g.frames
            .iter()
            .find(|f| f.index == index)
            .cloned()
    }

    pub fn latest(&self) -> Option<Frame> {
        self.inner.lock().unwrap().frames.back().cloned()
    }

    /// 只要某一帧的源时间，不复制像素——走 `get` 会白白拷 691 KB。
    /// 调度器不需要它（下一帧的时间由 `frame_to_secs(cursor + 1)` 直接算），
    /// 现在是测试在用。
    pub fn secs_at(&self, index: i64) -> Option<f64> {
        self.inner
            .lock()
            .unwrap()
            .frames
            .iter()
            .find(|f| f.index == index)
            .map(|f| f.secs)
    }

    /// 取"现在该显示的那一帧"：比当前游标新、源时间又不超过 `limit_secs` 的最新一帧。
    ///
    /// 从尾部往回扫，遇到的第一个就是要的那帧。取到就把游标推到它身上——淘汰和
    /// 背压都按游标算，所以这一步同时也是"我显示到这儿了"的通知，读线程该知道
    /// 前面的旧帧可以扔了。
    pub fn take_due(&self, limit_secs: f64) -> Option<Frame> {
        let mut g = self.inner.lock().unwrap();
        let after = g.cursor;
        let f = g
            .frames
            .iter()
            .rev()
            .find(|f| f.index > after && f.secs <= limit_secs)
            .cloned();
        if let Some(ref got) = f {
            g.cursor = got.index;
            self.evict(&mut g);
        }
        drop(g);
        if f.is_some() {
            self.cv_space.notify_all();
        }
        f
    }

    /// 等某一帧解出来。逐帧走缓冲够不着的时候用（比如刚跳转完、环还是空的）。
    pub fn wait_for_index(&self, index: i64, timeout: Duration) -> Option<Frame> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(f) = self.get(index) {
                return Some(f);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let g = self.inner.lock().unwrap();
            // 拿不到就等一小会儿再看一眼；guard 在语句结束时就放掉了
            let _ = self.cv_data.wait_timeout(g, deadline - now).unwrap();
        }
    }
}

// ------------------------------------------------------------------ PCM 环

/// PCM 字节的环形缓冲。和视频那个一样，满了就顶住读线程，让 ffmpeg 停在那。
pub struct PcmRing {
    inner: Mutex<VecDeque<u8>>,
    cv: Condvar,
    limit: usize,
    frame_bytes: usize,
    aborted: AtomicBool,
    eof: AtomicBool,
    total: AtomicU64,
}

impl PcmRing {
    pub fn new(limit_bytes: usize, frame_bytes: usize) -> Arc<PcmRing> {
        Arc::new(PcmRing {
            inner: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            limit: limit_bytes,
            frame_bytes: frame_bytes.max(1),
            aborted: AtomicBool::new(false),
            eof: AtomicBool::new(false),
            total: AtomicU64::new(0),
        })
    }

    fn push(&self, data: &[u8]) -> bool {
        let mut g = self.inner.lock().unwrap();
        while g.len() > self.limit {
            if self.aborted.load(Ordering::SeqCst) {
                return false;
            }
            let (ng, _) = self
                .cv
                .wait_timeout(g, Duration::from_millis(50))
                .unwrap();
            g = ng;
        }
        if self.aborted.load(Ordering::SeqCst) {
            return false;
        }
        g.extend(data.iter());
        self.total.fetch_add(data.len() as u64, Ordering::Relaxed);
        drop(g);
        self.cv.notify_all();
        true
    }

    /// 往设备缓冲里拷，只拷整帧。返回真正拷了多少字节。
    ///
    /// # Safety
    /// `dst` 必须能写下 `need` 字节。
    pub unsafe fn fill(&self, dst: *mut u8, need: usize) -> usize {
        let mut g = self.inner.lock().unwrap();
        let have = g.len().min(need);
        let usable = (have / self.frame_bytes) * self.frame_bytes;
        if usable > 0 {
            let (a, b) = g.as_slices();
            let n1 = a.len().min(usable);
            std::ptr::copy_nonoverlapping(a.as_ptr(), dst, n1);
            if usable > n1 {
                std::ptr::copy_nonoverlapping(b.as_ptr(), dst.add(n1), usable - n1);
            }
            g.drain(..usable);
        }
        let got = usable;
        drop(g);
        self.cv.notify_all();
        got
    }

    pub fn backlog_bytes(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn backlog_ms(&self, bytes_per_sec: f64) -> f64 {
        if bytes_per_sec <= 0.0 {
            return 0.0;
        }
        self.backlog_bytes() as f64 / bytes_per_sec * 1000.0
    }

    /// 等积压到 `bytes`，或者等到 EOF。返回是否达标。
    pub fn wait_backlog(&self, bytes: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut g = self.inner.lock().unwrap();
        loop {
            if g.len() >= bytes || self.eof.load(Ordering::SeqCst) {
                return g.len() >= bytes;
            }
            let now = Instant::now();
            if now >= deadline || self.aborted.load(Ordering::SeqCst) {
                return false;
            }
            let (ng, _) = self.cv.wait_timeout(g, deadline - now).unwrap();
            g = ng;
        }
    }

    pub fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        self.cv.notify_all();
    }

    fn finish_eof(&self) {
        self.eof.store(true, Ordering::SeqCst);
        self.cv.notify_all();
    }

    pub fn is_eof(&self) -> bool {
        self.eof.load(Ordering::SeqCst)
    }

    pub fn total_bytes(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

// ------------------------------------------------------------------ 会话

/// 起一次会话要的所有东西。
pub struct SourceSpec {
    pub ffmpeg: String,
    pub input: String,
    /// 从源文件的第几秒开始
    pub seek: f64,
    /// `seek` 落在第几帧（调用方用 `Framing::secs_to_frame` 算好）
    pub frame0: i64,
    pub frame_secs: FrameSecs,
    /// 没有音轨就给 None
    pub audio: Option<AudioSpec>,
}

/// 一次播放会话：两个子进程、两条读数线程、两个环形缓冲。
///
/// 跳转就是把这个整个丢掉再造一个。子进程从 `-ss` 开始解码，本地时间轴从 0 起。
pub struct Session {
    kids: Kids,
    frames: Arc<FrameRing>,
    pcm: Option<Arc<PcmRing>>,
    video_thread: Option<JoinHandle<()>>,
    pcm_thread: Option<JoinHandle<()>>,
    pub seek: f64,
    pub frame0: i64,
    pub input: String,
}

struct Kids {
    video: Child,
    audio: Option<Child>,
}

impl Kids {
    /// 杀掉并收尸。可以重复调：已经收过的进程再 kill / wait 只会返回个错误。
    fn kill(&mut self) {
        let _ = self.video.kill();
        let _ = self.video.wait();
        if let Some(a) = self.audio.as_mut() {
            let _ = a.kill();
            let _ = a.wait();
        }
    }
}

impl Drop for Kids {
    /// `Session::start` 中途 `?` 返回时子进程还得有人管。`Child` 自己的 `Drop`
    /// 只关句柄，不动进程，所以没有这个的话已经把文件读起来的那个 ffmpeg 会一直留着。
    fn drop(&mut self) {
        self.kill();
    }
}

impl Session {
    /// 起两个子进程和两条读数线程。`pcm_limit_bytes` 是 PCM 环的上限，
    /// 建议给 4 秒的量。
    pub fn start(spec: SourceSpec, pcm_limit_bytes: usize) -> io::Result<Session> {
        let mut video_child = spawn(&spec.ffmpeg, &video_args(&spec.input, spec.seek))?;
        let video_out = video_child.stdout.take().expect("video stdout");
        // 一出生就交给 `Kids` 管：下面每一条 `?` 提前返回时，这个局部变量被丢掉
        // 就会把已经起起来的进程一起收掉。
        let mut kids = Kids {
            video: video_child,
            audio: None,
        };

        let audio_child = match &spec.audio {
            Some(a) => {
                let mut c = spawn(&spec.ffmpeg, &audio_args(&spec.input, spec.seek, a))?;
                let out = c.stdout.take().expect("audio stdout");
                kids.audio = Some(c);
                Some(out)
            }
            None => None,
        };

        let frames = FrameRing::new(RING_FRAMES, spec.frame0);
        let video_thread = {
            let ring = frames.clone();
            let map = spec.frame_secs.clone();
            let frame0 = spec.frame0;
            thread::Builder::new()
                .name("preview-vframe".into())
                .spawn(move || read_video(video_out, ring, frame0, map))?
        };

        let (pcm, pcm_thread) = match audio_child {
            Some(out) => {
                let frame_bytes = spec
                    .audio
                    .as_ref()
                    .map(|a| a.block_align())
                    .unwrap_or(4);
                let ring = PcmRing::new(pcm_limit_bytes, frame_bytes);
                let r2 = ring.clone();
                let h = thread::Builder::new()
                    .name("preview-pcm".into())
                    .spawn(move || read_pcm(out, r2))?;
                (Some(ring), Some(h))
            }
            None => (None, None),
        };

        Ok(Session {
            kids,
            frames,
            pcm,
            video_thread: Some(video_thread),
            pcm_thread,
            seek: spec.seek,
            frame0: spec.frame0,
            input: spec.input,
        })
    }

    pub fn frames(&self) -> &Arc<FrameRing> {
        &self.frames
    }

    pub fn pcm(&self) -> Option<&Arc<PcmRing>> {
        self.pcm.as_ref()
    }

    /// 关掉子进程和读数线程。可以重复调。
    pub fn stop(&mut self) {
        self.frames.abort();
        if let Some(p) = &self.pcm {
            p.abort();
        }
        self.kids.kill();
        if let Some(h) = self.video_thread.take() {
            let _ = h.join();
        }
        if let Some(h) = self.pcm_thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 按整帧读视频管道。`read_exact` 之前先分配好像素缓冲，读进去再移交给环形缓冲，
/// 全程只拷一次（内核到用户态）。
fn read_video(
    mut out: std::process::ChildStdout,
    ring: Arc<FrameRing>,
    frame0: i64,
    map: FrameSecs,
) {
    let mut i = 0i64;
    loop {
        let mut pixels = SharedPixelBuffer::<Rgb8Pixel>::new(FRAME_W as u32, FRAME_H as u32);
        if read_exact(&mut out, pixels.make_mut_bytes()).is_err() {
            break;
        }
        let index = frame0 + i;
        let secs = map(index);
        if !ring.push(Frame {
            index,
            secs,
            pixels,
        }) {
            break;
        }
        i += 1;
    }
    ring.finish_eof();
}

fn read_pcm(mut out: std::process::ChildStdout, ring: Arc<PcmRing>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match out.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if !ring.push(&buf[..n]) {
                    break;
                }
            }
        }
    }
    ring.finish_eof();
}

/// 读满整个 buffer。EOF 或者读一半断了都算失败，调用方按整帧对齐，不能拿到半帧。
fn read_exact(f: &mut impl Read, buf: &mut [u8]) -> io::Result<()> {
    let mut got = 0;
    while got < buf.len() {
        match f.read(&mut buf[got..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "视频管道提前结束",
                ))
            }
            Ok(n) => got += n,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一帧最小尺寸的假画面。环形缓冲不看像素内容，1×1 就够，
    /// 省得每个测试都去分配 691 KB。
    fn frame(index: i64, secs: f64) -> Frame {
        Frame {
            index,
            secs,
            pixels: SharedPixelBuffer::<Rgb8Pixel>::new(1, 1),
        }
    }

    /// 第 `index` 帧的时间，按 24 fps 算
    fn at(index: i64) -> f64 {
        index as f64 / 24.0
    }

    // ---------------------------------------------------------- 命令行

    #[test]
    fn fmt_secs_writes_plain_decimal_every_time() {
        assert_eq!(fmt_secs(30.5), "30.500000");
        assert_eq!(fmt_secs(0.0), "0.000000");
        assert_eq!(fmt_secs(1234.5678901), "1234.567890");
        // 负数当 0：让 ffmpeg 去解释负的定位只会得到别的东西
        assert_eq!(fmt_secs(-3.0), "0.000000");
        // 小数点永远是点。要是跟了区域设置变成逗号，ffmpeg 会把它当参数分隔符
        assert!(fmt_secs(1.25).contains('.'));
        // 别出现科学计数法，ffmpeg 不认 "1e-7"
        assert!(!fmt_secs(1e-9).contains('e'));
        assert_eq!(fmt_secs(1e-9), "0.000000");
    }

    #[test]
    fn video_args_pin_the_two_things_the_timeline_depends_on() {
        let a = video_args("C:\\v\\a.mp4", 30.0);
        let joined = a.join(" ");

        // `-ss` 在 `-i` 前面才是输入定位：帧号准，而且不用先解到那儿
        let ss = a.iter().position(|s| s == "-ss").unwrap();
        let i = a.iter().position(|s| s == "-i").unwrap();
        assert!(ss < i, "-ss 得在 -i 前面");
        assert_eq!(a[ss + 1], "30.000000");

        // 少了 passthrough，ffmpeg 会按自己的帧率策略补帧丢帧，
        // 那"第 i 帧对应什么时间"整个文件就散架了
        assert!(
            a.windows(2).any(|w| w[0] == "-fps_mode:v" && w[1] == "passthrough"),
            "必须有 -fps_mode:v passthrough"
        );

        // 管道里的 rawvideo 没有逐帧头，尺寸必须写死
        assert!(joined.contains("scale=640:360:force_original_aspect_ratio=decrease"));
        assert!(joined.contains("pad=640:360:(ow-iw)/2:(oh-ih)/2"));
        assert_eq!(FRAME_BYTES, 640 * 360 * 3);

        assert!(a.windows(2).any(|w| w[0] == "-f" && w[1] == "rawvideo"));
        assert!(a.windows(2).any(|w| w[0] == "-map" && w[1] == "0:v:0"));
        assert!(a.contains(&"-an".to_string()), "音频走另一个子进程");
        assert_eq!(a.last().unwrap(), "pipe:1");
    }

    #[test]
    fn video_args_seek_keeps_microsecond_precision() {
        let a = video_args("in.mp4", 0.000001);
        let ss = a.iter().position(|s| s == "-ss").unwrap();
        assert_eq!(a[ss + 1], "0.000001");
    }

    #[test]
    fn audio_args_follow_the_negotiated_format() {
        let spec = AudioSpec {
            rate: 48_000,
            channels: 2,
            sample: "f32".into(),
        };
        let a = audio_args("in.mp4", 12.5, &spec);
        let joined = a.join(" ");

        assert!(a.windows(2).any(|w| w[0] == "-c:a" && w[1] == "pcm_f32le"));
        assert!(a.windows(2).any(|w| w[0] == "-f" && w[1] == "f32le"));
        assert!(a.windows(2).any(|w| w[0] == "-ar" && w[1] == "48000"));
        assert!(a.windows(2).any(|w| w[0] == "-ac" && w[1] == "2"));
        assert!(joined.contains("aformat=sample_fmts=f32:sample_rates=48000:channel_layouts=stereo"));
        // first_pts=0：源文件的声音比画面晚起时补一段等长的静音，
        // 而不是把声音整体往前挪
        assert!(joined.contains("aresample=48000:async=1:first_pts=0"));

        assert!(a.windows(2).any(|w| w[0] == "-map" && w[1] == "0:a:0"));
        assert!(a.contains(&"-vn".to_string()), "画面走另一个子进程");
        assert_eq!(a.last().unwrap(), "pipe:1");
    }

    #[test]
    fn audio_args_switch_to_s16_when_that_is_what_the_device_took() {
        let spec = AudioSpec {
            rate: 44_100,
            channels: 1,
            sample: "s16".into(),
        };
        let a = audio_args("in.mp4", 0.0, &spec);
        let joined = a.join(" ");

        assert!(a.windows(2).any(|w| w[0] == "-c:a" && w[1] == "pcm_s16le"));
        assert!(a.windows(2).any(|w| w[0] == "-f" && w[1] == "s16le"));
        assert!(joined.contains("sample_rates=44100"));
        assert!(joined.contains("channel_layouts=mono"));
    }

    #[test]
    fn audio_spec_derives_the_rest_from_the_sample_name() {
        let s16 = AudioSpec {
            rate: 48_000,
            channels: 2,
            sample: "s16".into(),
        };
        let f32 = AudioSpec {
            rate: 48_000,
            channels: 2,
            sample: "f32".into(),
        };
        let solo = AudioSpec {
            rate: 48_000,
            channels: 1,
            sample: "s16".into(),
        };

        assert_eq!(s16.block_align(), 4);
        assert_eq!(f32.block_align(), 8);
        assert_eq!(solo.block_align(), 2);

        assert_eq!(s16.codec(), "pcm_s16le");
        assert_eq!(f32.codec(), "pcm_f32le");
        assert_eq!(s16.muxer(), "s16le");
        assert_eq!(f32.muxer(), "f32le");

        assert_eq!(solo.layout(), "mono");
        assert_eq!(s16.layout(), "stereo");
        // 不认识的声道数回落到立体声，别拼出 ffmpeg 不认的布局名
        assert_eq!(
            AudioSpec {
                rate: 48_000,
                channels: 3,
                sample: "s16".into()
            }
            .layout(),
            "stereo"
        );
    }

    // ---------------------------------------------------------- 解码帧环

    #[test]
    fn ring_keeps_the_history_window_and_drops_what_is_behind_it() {
        let ring = FrameRing::new(96, 0);
        for index in 0..96 {
            assert!(ring.push(frame(index, at(index))));
        }
        // 一帧都还没显示过，游标停在 0 前面，淘汰范围够不到第 0 帧
        assert_eq!(ring.front_index(), Some(0));
        assert_eq!(ring.cursor(), -1);

        ring.set_cursor(95);
        // 显示到 95 了，往回只够得着 48 帧，47 以前的留着也没用
        assert_eq!(ring.front_index(), Some(47));
        assert_eq!(ring.back_index(), Some(95));
    }

    #[test]
    fn ring_unblocks_the_reader_once_the_display_position_moves_on() {
        let ring = FrameRing::new(96, 0);
        for index in 0..96 {
            assert!(ring.push(frame(index, at(index))));
        }
        assert_eq!(ring.backlog(), 96, "环已经满了");

        // 第 97 帧进不来，读线程会停在 condvar 上
        let reader = Arc::clone(&ring);
        let blocked = std::thread::spawn(move || reader.push(frame(96, at(96))));

        // 显示位置一往前走，48 帧旧历史被淘汰，位置就腾出来了
        ring.set_cursor(96);
        assert!(blocked.join().unwrap(), "腾出位置之后读线程得醒过来");
        assert_eq!(ring.back_index(), Some(96));
        assert_eq!(ring.backlog(), 49, "淘汰 48 帧，又进来 1 帧");
    }

    #[test]
    fn abort_wakes_a_reader_stuck_on_a_full_ring() {
        let ring = FrameRing::new(16, 0);
        for index in 0..16 {
            assert!(ring.push(frame(index, at(index))));
        }

        let reader = Arc::clone(&ring);
        let blocked = std::thread::spawn(move || reader.push(frame(16, at(16))));
        // 等它真的停下来等，不然测的就不是"被唤醒"了
        std::thread::sleep(Duration::from_millis(80));

        ring.abort();
        assert!(!blocked.join().unwrap(), "abort 之后 push 要返回 false 让读线程收工");
    }

    #[test]
    fn take_due_hands_over_the_newest_frame_whose_time_has_come() {
        let ring = FrameRing::new(96, 0);
        for index in 0..10 {
            assert!(ring.push(frame(index, at(index))));
        }

        // 第 0..3 帧的时间都到了，但只给最新的那一帧：中间那些是掉帧，不是要逐帧补放
        let got = ring.take_due(at(3)).unwrap();
        assert_eq!(got.index, 3);
        assert_eq!(ring.cursor(), 3, "取走的同时把显示位置推上去");

        // 没有更新的帧到点，就什么也不给，别把同一帧再发一遍
        assert!(ring.take_due(at(3)).is_none());

        let got = ring.take_due(at(9)).unwrap();
        assert_eq!(got.index, 9);
    }

    #[test]
    fn take_due_leaves_frames_that_are_still_in_the_future() {
        let ring = FrameRing::new(96, 0);
        for index in 0..5 {
            assert!(ring.push(frame(index, at(index))));
        }
        // 还差一点点才到第 4 帧，只能给第 3 帧
        let got = ring.take_due(at(4) - 0.0001).unwrap();
        assert_eq!(got.index, 3);
    }

    #[test]
    fn take_due_expects_source_times_not_session_times() {
        // 环里存的 secs 是源文件时间（framing.frame_to_secs(frame0 + i)），
        // 调度线程喂进来的上限是 seek + 本地时间，两边在同一个轴上。
        //
        // 谁要是把 secs 改成"会话相对时间"（每次跳转从 0 重新数），
        // 跳转之后 take_due 会认为整个环都已经到点，一口气把 96 帧全推出去，
        // 帧号也会整个归零。这个测试就是拦这件事的。
        let seek = 120.0;
        let f0 = 7200; // 120 秒 × 60 fps
        let ring = FrameRing::new(96, f0);
        for i in 0..10 {
            let index = f0 + i;
            assert!(ring.push(frame(index, index as f64 / 60.0)));
        }

        // 本地时间才走到 0.045 秒（夹在第 2、3 帧中间，离两边都有 5 ms 余量），
        // 只该给到源时间 120.033 那一帧
        let got = ring.take_due(seek + 0.045).unwrap();
        assert_eq!(got.index, f0 + 2);
        assert!(
            got.secs >= seek,
            "发布出去的时间必须还在源时间轴上，不能回到 0 附近"
        );
    }

    #[test]
    fn ring_lookup_says_nothing_for_a_frame_that_is_not_there() {        let ring = FrameRing::new(8, 100);
        assert!(ring.push(frame(100, at(100))));

        assert_eq!(ring.get(100).map(|f| f.index), Some(100));
        assert!(ring.get(99).is_none());
        assert_eq!(ring.latest().map(|f| f.index), Some(100));
        assert_eq!(ring.secs_at(100), Some(at(100)));
        assert_eq!(ring.secs_at(101), None);
        assert_eq!(ring.frame0(), 100);
        assert_eq!(ring.decoded(), 1);
    }

    #[test]
    fn wait_for_index_returns_at_once_or_gives_up() {
        let ring = FrameRing::new(8, 0);
        assert!(ring.push(frame(5, at(5))));
        assert_eq!(
            ring.wait_for_index(5, Duration::from_millis(50)).map(|f| f.index),
            Some(5)
        );

        // 等不到的帧不能把调用方挂在那儿
        let started = Instant::now();
        assert!(ring.wait_for_index(9, Duration::from_millis(60)).is_none());
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    // ---------------------------------------------------------- PCM 环

    #[test]
    fn pcm_fill_only_hands_over_whole_frames() {
        // 一个采样 4 字节（48 kHz 立体声 s16）
        let ring = PcmRing::new(64, 4);
        assert!(ring.push(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]));

        let mut dst = [0u8; 8];
        let got = unsafe { ring.fill(dst.as_mut_ptr(), 8) };
        assert_eq!(got, 8);
        assert_eq!(dst, [1, 2, 3, 4, 5, 6, 7, 8]);

        // 剩下 2 字节凑不满一帧，留在环里等下一批——
        // 半帧写进设备缓冲就是爆音
        assert_eq!(ring.backlog_bytes(), 2);
        assert_eq!(ring.total_bytes(), 10, "total 记的是进来多少，不是被取走多少");
    }

    #[test]
    fn pcm_fill_gives_what_there_is_when_asked_for_more() {
        let ring = PcmRing::new(64, 4);
        assert!(ring.push(&[1, 2, 3, 4]));

        let mut dst = [0u8; 16];
        let got = unsafe { ring.fill(dst.as_mut_ptr(), 16) };
        assert_eq!(got, 4, "只有一帧的量就只给一帧");
        assert_eq!(ring.backlog_bytes(), 0);
    }

    #[test]
    fn pcm_backlog_ms_converts_with_the_byte_rate() {
        let ring = PcmRing::new(4096, 4);
        // 48 kHz 立体声 s16 每秒 192000 字节
        assert!(ring.push(&vec![0u8; 19_200]));
        assert_eq!(ring.backlog_ms(192_000.0), 100.0);
        assert_eq!(ring.backlog_ms(0.0), 0.0, "字节率无效时别除出个 inf");
    }

    #[test]
    fn pcm_wait_backlog_times_out_and_also_gives_up_on_abort() {
        let ring = PcmRing::new(64, 4);
        assert!(
            !ring.wait_backlog(16, Duration::from_millis(40)),
            "没攒够就是没攒够"
        );

        let waiting = Arc::clone(&ring);
        let waiter = std::thread::spawn(move || waiting.wait_backlog(16, Duration::from_secs(30)));
        std::thread::sleep(Duration::from_millis(60));

        ring.abort();
        assert!(!waiter.join().unwrap(), "abort 之后不能还挂在那儿等半分钟");
    }

    // ---------------------------------------------------------- 跨线程

    #[test]
    fn decoded_frames_can_cross_into_the_ui_thread() {
        // 解码线程做出来的帧要交给 UI 线程。这两个类型要是变成不 Send，
        // 整条链路就得推倒重来，所以在这儿用编译期断言钉死。
        fn assert_send<T: Send>() {}
        assert_send::<Frame>();
        assert_send::<SharedPixelBuffer<Rgb8Pixel>>();

        // `slint::Image` 反过来是不 Send 的（里面那个 OpaqueImage 分支带
        // 裸指针 vtable），所以跨线程传的必须是 `Frame`，`to_image()` 只能
        // 在 UI 线程上调。app.rs 的 sync_preview 就是按这个来的，
        // 谁要是把 to_image() 挪到解码线程去，这儿是拦不住的——只能靠注释。
    }
}
