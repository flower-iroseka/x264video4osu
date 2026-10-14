//! WASAPI 共享模式出声：格式协商、主时钟、渲染线程、音量。
//!
//! 音频是主时钟。`IAudioClock` 报的是"设备已经播到第几个采样"，锚点换算成媒体时间：
//!
//! ```text
//! local_us = T0 + (P - P0) * 1_000_000 / F
//! ```
//!
//! `local_us` 是**本次会话内**的时间，每次跳转从 0 重新开始；绝对媒体时间由调用方
//! 加上 `SEEK`。`IAudioClient` 和 `IAudioClock` 整个预览会话只建一次，跳转时只换锚点，
//! 不重建设备——重建除了慢，还会引入新的音画不同步。
//!
//! 这个模块**不引用 `crate::` 里的任何东西**（PCM 来源是一段闭包），这样
//! `examples/preview_ffmpeg_spike.rs` 能用 `#[path]` 把同一个源文件 include 进去，
//! 那个验证程序跑的就是产品要用的那份代码。
//!
//! ## 几个踩过的点，改之前先看
//!
//! * `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` 是 packed(1) 的，字段一律不能取引用
//!   （连 `format!` 都会借），统一按字节偏移读出来存进普通结构体。
//! * `Reset()` 只在停止状态合法，所以顺序只能是 `Stop()` → `Reset()`。
//! * 停止状态下 `GetBuffer` / `ReleaseBuffer` 是合法的，第一次 `Start()` 前的标准
//!   流程就是先把设备缓冲填满。这里就靠它做"先攒后启动"：不这么做的话，渲染引擎在
//!   等 ffmpeg 出声音的那段时间里播的是零，而时钟照走，媒体时间会比第一个真实采样
//!   早一百多毫秒。
//! * `IAudioClock` 的位置只在运行时走动。停机期间一律用冻结值，不要再去问设备。

// 产品只用到其中一部分：协商出来的格式、设备周期这些是给
// examples/preview_ffmpeg_spike.rs 看数据的，这里先压着别报未使用。
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioClock, IAudioRenderClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// 设备缓冲时长。100 ms 和那份工作台探针一致，够吸收调度抖动，又不至于让跳转后
/// 的旧内容多留一截。
const BUFFER_HNS: i64 = 1_000_000;

/// 计划里写死的格式。设备不接受就回落到设备混音格式。
const PLAN_RATE: u32 = 48_000;
const PLAN_CHANNELS: u16 = 2;

/// 把系统定时器精度提到 1 ms。默认 15.6 ms 的话，渲染线程的等待粒度会把
/// 排期精度整个吃掉。用完调 [`lower_timer_resolution`] 还回去。
pub fn raise_timer_resolution() {
    unsafe {
        timeBeginPeriod(1);
    }
}

pub fn lower_timer_resolution() {
    unsafe {
        timeEndPeriod(1);
    }
}

/// 一次会话开始前调一次。MTA 下所有音频接口都能跨线程用，渲染线程不必自己初始化 COM。
pub fn init_com() -> Result<(), String> {
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() {
        return Err(format!("CoInitializeEx(MTA): {:#x}", hr.0));
    }
    Ok(())
}

pub fn uninit_com() {
    unsafe {
        CoUninitialize();
    }
}

// ------------------------------------------------------------------ 音频格式

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SampleFmt {
    S16,
    F32,
}

/// 协商出来、真正开流用的格式。ffmpeg 的音频命令行要按它来拼。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AudioFormat {
    pub rate: u32,
    pub channels: u16,
    pub sample: SampleFmt,
    pub block_align: usize,
}

impl AudioFormat {
    pub fn bytes_per_sec(&self) -> f64 {
        self.rate as f64 * self.block_align as f64
    }

    /// `aformat` 滤镜里的采样格式名。ffmpeg 命令行的其余部分（编码器、muxer、
    /// 声道布局）由 `preview_media::AudioSpec` 从这一个名字推出来，
    /// 免得同一个格式在两处各写一份。
    pub fn ffmpeg_sample(&self) -> &'static str {
        match self.sample {
            SampleFmt::S16 => "s16",
            SampleFmt::F32 => "f32",
        }
    }

    /// 就是计划里写死的 48000 / s16 / 立体声
    pub fn is_plan(&self) -> bool {
        self.rate == PLAN_RATE
            && self.channels == PLAN_CHANNELS
            && self.sample == SampleFmt::S16
    }

    pub fn describe(&self) -> String {
        format!(
            "{} Hz / {} ch / {:?} / block {}",
            self.rate, self.channels, self.sample, self.block_align
        )
    }
}

unsafe fn rd_u16(b: *const u8, off: usize) -> u16 {
    std::ptr::read_unaligned(b.add(off) as *const u16)
}
unsafe fn rd_u32(b: *const u8, off: usize) -> u32 {
    std::ptr::read_unaligned(b.add(off) as *const u32)
}

const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// `WAVEFORMATEX` 的好读副本。字段不能直接取引用（结构体是 packed 的）。
#[derive(Clone, Copy)]
struct Fmt {
    tag: u16,
    channels: u16,
    rate: u32,
    bits: u16,
    block_align: u16,
    cb: u16,
    mask: u32,
    sub: GUID,
}

impl Fmt {
    unsafe fn read(p: *const WAVEFORMATEX) -> Fmt {
        let b = p as *const u8;
        let mut f = Fmt {
            tag: rd_u16(b, 0),
            channels: rd_u16(b, 2),
            rate: rd_u32(b, 4),
            bits: rd_u16(b, 14),
            block_align: rd_u16(b, 12),
            cb: rd_u16(b, 16),
            mask: 0,
            sub: GUID::from_u128(0),
        };
        // WAVEFORMATEXTENSIBLE 是 18 字节的 WAVEFORMATEX 后面接
        // wValidBitsPerSample(2) + dwChannelMask(4) + SubFormat(16)
        if f.tag == WAVE_FORMAT_EXTENSIBLE as u16 && f.cb >= 22 {
            f.mask = rd_u32(b, 20);
            f.sub = std::ptr::read_unaligned(b.add(24) as *const GUID);
        }
        f
    }

    fn sample_fmt(&self) -> SampleFmt {
        let float = if self.tag == WAVE_FORMAT_EXTENSIBLE as u16 {
            self.sub == SUBTYPE_IEEE_FLOAT
        } else {
            self.tag == 3 // WAVE_FORMAT_IEEE_FLOAT
        };
        if float {
            SampleFmt::F32
        } else {
            SampleFmt::S16
        }
    }

    fn describe(&self) -> String {
        let mut s = format!(
            "tag={:#x} ch={} rate={} bits={} block={}",
            self.tag, self.channels, self.rate, self.bits, self.block_align
        );
        if self.tag == WAVE_FORMAT_EXTENSIBLE as u16 {
            let kind = if self.sub == SUBTYPE_IEEE_FLOAT {
                "float"
            } else if self.sub == KSDATAFORMAT_SUBTYPE_PCM {
                "pcm"
            } else {
                "other"
            };
            s += &format!(" sub={} mask={:#x}", kind, self.mask);
        }
        s
    }
}

/// 传给 `Initialize` 的那块 `WAVEFORMATEX` 内存，外加一份好读的副本。
struct EngineFormat {
    mem: Box<WAVEFORMATEXTENSIBLE>,
    fmt: AudioFormat,
}

impl EngineFormat {
    fn wfx(&self) -> *const WAVEFORMATEX {
        &*self.mem as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX
    }

    /// 计划里写死的 48000 / s16 / 立体声
    fn plan() -> EngineFormat {
        let mut mem = Box::new(WAVEFORMATEXTENSIBLE::default());
        mem.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE as u16;
        mem.Format.nChannels = PLAN_CHANNELS;
        mem.Format.nSamplesPerSec = PLAN_RATE;
        mem.Format.nAvgBytesPerSec = PLAN_RATE * 4;
        mem.Format.nBlockAlign = 4;
        mem.Format.wBitsPerSample = 16;
        mem.Format.cbSize = 22;
        mem.Samples.wValidBitsPerSample = 16;
        mem.dwChannelMask = 0x3; // 左前 | 右前
        mem.SubFormat = KSDATAFORMAT_SUBTYPE_PCM;
        let fmt = AudioFormat {
            rate: PLAN_RATE,
            channels: PLAN_CHANNELS,
            sample: SampleFmt::S16,
            block_align: 4,
        };
        EngineFormat { mem, fmt }
    }

    /// 把设备混音格式原样抄一份
    unsafe fn from_mix(mix: *const WAVEFORMATEX) -> EngineFormat {
        let raw = Fmt::read(mix);
        let mut mem = Box::new(WAVEFORMATEXTENSIBLE::default());
        let n = (18 + raw.cb as usize).min(std::mem::size_of::<WAVEFORMATEXTENSIBLE>());
        std::ptr::copy_nonoverlapping(
            mix as *const u8,
            &mut *mem as *mut WAVEFORMATEXTENSIBLE as *mut u8,
            n,
        );
        let fmt = AudioFormat {
            rate: raw.rate,
            channels: raw.channels,
            sample: raw.sample_fmt(),
            block_align: raw.block_align as usize,
        };
        EngineFormat { mem, fmt }
    }
}

/// 设备到底接受了哪个格式（spike 的门槛第 9 条要看这个）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Negotiation {
    /// 计划里写死的 48000/s16/stereo 被接受
    Plan,
    /// 回落到设备混音格式
    MixFormat(String),
}

// ------------------------------------------------------------------ 引擎

struct Base {
    t0_us: i64,
    p0: u64,
}

/// 锚点换算：设备已经播到第 `p` 个采样时，会话内的媒体时间是第几微秒。
///
/// 减法要在 i128 上做：`p` 比 `p0` 小时（锚点是 `Start()` 之后才采的，
/// 中间设备可能已经往回挪过），两个 u64 直接相减会绕成一个天文数字。
/// 落到 i128 里才谈得上"负数"，后面夹到 0 才有意义。
///
/// `freq` 是 `IAudioClock::GetFrequency` 报的采样率，正常不可能是 0；
/// 真拿到 0 就当作时钟不走，停在 `t0_us`。
fn media_time_us(t0_us: i64, p0: u64, p: u64, freq: u64) -> i64 {
    if freq == 0 {
        return t0_us.max(0);
    }
    let delta = (p as i128 - p0 as i128) * 1_000_000i128 / freq as i128;
    (t0_us as i128 + delta).max(0) as i64
}

/// 往设备缓冲里填 PCM 的来源。返回值是真正写进去的字节数（整帧）。
///
/// 用闭包而不是具体类型，是为了让这个模块不依赖 `preview_media`，
/// 两边都能被 example 单独 include。
pub type PcmFill = Arc<dyn Fn(*mut u8, usize) -> usize + Send + Sync>;

pub struct AudioEngine {
    client: IAudioClient,
    render: IAudioRenderClient,
    clock: IAudioClock,
    event: HANDLE,
    freq: u64,
    buffer_frames: u32,
    fmt: AudioFormat,
    negotiation: Negotiation,
    start: Instant,

    running: AtomicBool,
    frozen_us: AtomicI64,
    base: Mutex<Base>,
    source: Mutex<Option<PcmFill>>,

    /// 音量，0.0..=100.0 之间，直接线性乘到样本上
    volume: AtomicU32,
    stop: AtomicBool,
    /// 填缓冲用，保证渲染线程和 prefill 不会同时进 GetBuffer
    fill_lock: Mutex<()>,

    // 统计
    pub underruns: AtomicU64,
    pub silence_frames: AtomicU64,
    pub worst_gap_ns: AtomicU64,
    pub callbacks: AtomicU64,
}

// COM 接口本身是自由的（MTA 下任何线程都能调），只是类型上没有 Send 标记。
unsafe impl Send for AudioEngine {}
unsafe impl Sync for AudioEngine {}

impl AudioEngine {
    /// 建一次设备、协商格式、开流。整个过程只做一次，跳转时不再调。
    pub fn open() -> Result<Arc<AudioEngine>, String> {
        unsafe { Self::open_inner() }
    }

    unsafe fn open_inner() -> Result<Arc<AudioEngine>, String> {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("CoCreateInstance(MMDeviceEnumerator): {e}"))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| format!("GetDefaultAudioEndpoint: {e}（没有默认输出设备？）"))?;
        let client: IAudioClient = device
            .Activate::<IAudioClient>(CLSCTX_ALL, None)
            .map_err(|e| format!("Activate(IAudioClient): {e}"))?;

        let mix = client
            .GetMixFormat()
            .map_err(|e| format!("GetMixFormat: {e}"))?;
        let mix_desc = Fmt::read(mix).describe();

        let plan = EngineFormat::plan();
        let mut closest: *mut WAVEFORMATEX = std::ptr::null_mut();
        let supported =
            client.IsFormatSupported(AUDCLNT_SHAREMODE_SHARED, plan.wfx(), Some(&mut closest));
        if !closest.is_null() {
            CoTaskMemFree(Some(closest as *const _));
        }

        let (engine_fmt, negotiation) = if supported.is_ok() {
            (EngineFormat::plan(), Negotiation::Plan)
        } else {
            (
                EngineFormat::from_mix(mix),
                Negotiation::MixFormat(mix_desc.clone()),
            )
        };
        CoTaskMemFree(Some(mix as *const _));

        let fmt = engine_fmt.fmt;
        let channel_mask_sane = fmt.channels > 0 && fmt.block_align > 0 && fmt.rate > 0;
        if !channel_mask_sane {
            return Err(format!("设备混音格式不可用: {}", fmt.describe()));
        }

        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                BUFFER_HNS,
                0,
                engine_fmt.wfx(),
                None,
            )
            .map_err(|e| format!("IAudioClient::Initialize: {e}"))?;

        let buffer_frames = client.GetBufferSize().map_err(|e| format!("GetBufferSize: {e}"))?;
        let clock: IAudioClock = client
            .GetService()
            .map_err(|e| format!("GetService(IAudioClock): {e}"))?;
        let freq = clock
            .GetFrequency()
            .map_err(|e| format!("IAudioClock::GetFrequency: {e}"))?;
        let render: IAudioRenderClient = client
            .GetService()
            .map_err(|e| format!("GetService(IAudioRenderClient): {e}"))?;

        let event: HANDLE = CreateEventW(None, false, false, PCWSTR::null())
            .map_err(|e| format!("CreateEventW: {e}"))?;
        client
            .SetEventHandle(event)
            .map_err(|e| format!("SetEventHandle: {e}"))?;

        Ok(Arc::new(AudioEngine {
            client,
            render,
            clock,
            event,
            freq,
            buffer_frames,
            fmt,
            negotiation,
            start: Instant::now(),
            running: AtomicBool::new(false),
            frozen_us: AtomicI64::new(0),
            base: Mutex::new(Base { t0_us: 0, p0: 0 }),
            source: Mutex::new(None),
            volume: AtomicU32::new(100.0f32.to_bits()),
            stop: AtomicBool::new(false),
            fill_lock: Mutex::new(()),
            underruns: AtomicU64::new(0),
            silence_frames: AtomicU64::new(0),
            worst_gap_ns: AtomicU64::new(0),
            callbacks: AtomicU64::new(0),
        }))
    }

    pub fn format(&self) -> AudioFormat {
        self.fmt
    }

    pub fn negotiation(&self) -> &Negotiation {
        &self.negotiation
    }

    pub fn buffer_frames(&self) -> u32 {
        self.buffer_frames
    }

    /// 设备缓冲折成毫秒
    pub fn buffer_ms(&self) -> f64 {
        self.buffer_frames as f64 * 1000.0 / self.fmt.rate as f64
    }

    /// 当前队列里有多少采样已经提交但还没播
    pub fn padding(&self) -> u32 {
        unsafe { self.client.GetCurrentPadding().unwrap_or(0) }
    }

    pub fn set_source(&self, fill: Option<PcmFill>) {
        *self.source.lock().unwrap() = fill;
    }

    fn position(&self) -> u64 {
        let mut p = 0u64;
        unsafe {
            let _ = self.clock.GetPosition(&mut p, None);
        }
        p
    }

    /// 会话内的媒体时间（微秒），跳转后从 0 重新开始。
    pub fn local_us(&self) -> i64 {
        if !self.running.load(Ordering::SeqCst) {
            return self.frozen_us.load(Ordering::SeqCst);
        }
        let p = self.position();
        let b = self.base.lock().unwrap();
        media_time_us(b.t0_us, b.p0, p, self.freq)
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// 起来或者从暂停恢复：先把设备缓冲攒满（[`Self::prefill`]），再调这个。
    pub fn start(&self) {
        if self.running.load(Ordering::SeqCst) {
            return;
        }
        unsafe {
            let _ = self.client.Start();
        }
        // 锚点必须在 Start 之后再采样：这样 P0 一定对应一个已经在走的时钟。
        // 采样前流逝的那一点点时间正好由公式里的 (P - P0) 吃掉。
        let p0 = self.position();
        *self.base.lock().unwrap() = Base {
            t0_us: self.frozen_us.load(Ordering::SeqCst),
            p0,
        };
        self.running.store(true, Ordering::SeqCst);
    }

    /// 暂停：把时钟冻在当前值，再停流。顺序不能反——停完就再也问不出位置了。
    pub fn pause(&self) {
        if !self.running.load(Ordering::SeqCst) {
            return;
        }
        let now = self.local_us();
        self.frozen_us.store(now, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
        unsafe {
            let _ = self.client.Stop();
        }
    }

    /// 跳转前调。把设备缓冲里还没播的旧音频整个扔掉，本地时间归零。
    /// 不扔的话每次跳转都会先听到约 100 ms 上一处的尾音。
    pub fn reset(&self) {
        self.pause();
        unsafe {
            let _ = self.client.Reset();
        }
        self.frozen_us.store(0, Ordering::SeqCst);
    }

    /// 停止状态下往设备缓冲里塞 PCM，塞满为止。返回值是这次真正提交的采样数。
    ///
    /// 设备缓冲本身就那么大，所以"攒满一个缓冲"和"攒到 padding == buffer_frames"
    /// 是一件事。够不着也没关系，调用方超时之后照常 Start。
    pub fn prefill(&self) -> u32 {
        let _guard = self.fill_lock.lock().unwrap();
        let padding = self.padding();
        if padding >= self.buffer_frames {
            return 0;
        }
        self.fill_host(true)
    }

    /// 采样格式和音量。返回 0..=1 的增益。
    fn gain(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed)) / 100.0
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    pub fn set_volume(&self, v: f32) {
        self.volume
            .store(v.clamp(0.0, 100.0).to_bits(), Ordering::Relaxed);
    }

    /// 往设备缓冲里填一次，返回提交的采样帧数。
    ///
    /// `prefill` 为真表示还在停机攒数据：只提交真正拿到了 PCM 的那部分，
    /// 剩下的不补零，`padding` 于是如实反映积压量。为假表示正在播：拿不到就补零，
    /// 那次补零就是一次 underrun，记进统计。
    fn fill_host(&self, prefill: bool) -> u32 {
        let padding = self.padding();
        if padding >= self.buffer_frames {
            return 0;
        }
        let avail = self.buffer_frames - padding;
        let ptr = match unsafe { self.render.GetBuffer(avail) } {
            Ok(p) => p,
            Err(_) => return 0,
        };

        let need = avail as usize * self.fmt.block_align;
        let source = self.source.lock().unwrap().clone();
        let got = match source {
            Some(f) => f(ptr, need),
            None => 0,
        };
        let got_frames = (got / self.fmt.block_align) as u32;

        let gain = self.gain();
        if gain != 1.0 && got_frames > 0 {
            unsafe { apply_gain(ptr, got_frames as usize, self.fmt, gain) };
        }

        if prefill {
            // 拿到 0 字节也必须 ReleaseBuffer(0)：缓冲区是被 GetBuffer 独占的，
            // 不还回去下一次 GetBuffer 就是 AUDCLNT_E_OUT_OF_ORDER，padding 永远
            // 停在 0，"攒够缓冲再启动"那套就整个死在这儿了。
            let _ = unsafe { self.render.ReleaseBuffer(got_frames, 0) };
            return got_frames;
        }

        if got_frames < avail {
            // 没 PCM 可填，只能补零。
            unsafe {
                std::ptr::write_bytes(ptr.add(got), 0, need - got);
            }
            let missing = (need - got) / self.fmt.block_align;
            self.underruns.fetch_add(1, Ordering::Relaxed);
            self.silence_frames
                .fetch_add(missing as u64, Ordering::Relaxed);
            let gap_ns = (missing as f64 / self.fmt.bytes_per_sec() * 1e9) as u64;
            self.worst_gap_ns.fetch_max(gap_ns, Ordering::Relaxed);
        }
        let _ = unsafe { self.render.ReleaseBuffer(avail, 0) };
        self.callbacks.fetch_add(1, Ordering::Relaxed);
        avail
    }

    /// 渲染线程。每来一次设备事件填一次缓冲，直到 [`Self::shutdown`]。
    pub fn render_loop(engine: &Arc<AudioEngine>) {
        loop {
            if engine.stop.load(Ordering::SeqCst) {
                break;
            }
            let waited = unsafe { WaitForSingleObject(engine.event, 100) };
            if engine.stop.load(Ordering::SeqCst) {
                break;
            }
            if waited != WAIT_OBJECT_0 {
                continue;
            }
            if !engine.running.load(Ordering::SeqCst) {
                continue;
            }
            let _guard = engine.fill_lock.lock().unwrap();
            engine.fill_host(false);
        }
    }

    /// 叫停渲染线程。返回后渲染线程最多再等 100 ms 就会退出，**调用方要 join
    /// 之后再丢这个 Arc**——句柄在 `Drop` 里关，渲染线程还在用的时候就关掉它会
    /// 踩到"关掉正在等待的句柄"。
    pub fn shutdown(&self) {
        self.pause();
        self.stop.store(true, Ordering::SeqCst);
    }

    /// 一次跳转之后要等 PCM 到齐再 Start，等待期间把能攒的先攒上。
    ///
    /// 返回是否攒够了一个完整缓冲。
    pub fn prime_and_start(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let target = self.buffer_frames;
        loop {
            self.prefill();
            if self.padding() >= target {
                self.start();
                return true;
            }
            if Instant::now() >= deadline {
                self.start();
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

/// 把已经拷进设备缓冲的那一段按音量缩放。
unsafe fn apply_gain(ptr: *mut u8, frames: usize, fmt: AudioFormat, gain: f32) {
    match fmt.sample {
        SampleFmt::S16 => {
            let p = ptr as *mut i16;
            for i in 0..frames * fmt.channels as usize {
                let s = std::ptr::read_unaligned(p.add(i)) as f32 * gain;
                let v = s.round().clamp(-32768.0, 32767.0) as i32;
                std::ptr::write_unaligned(p.add(i), v as i16);
            }
        }
        SampleFmt::F32 => {
            let p = ptr as *mut f32;
            for i in 0..frames * fmt.channels as usize {
                let s = std::ptr::read_unaligned(p.add(i)) * gain;
                std::ptr::write_unaligned(p.add(i), s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(sample: SampleFmt, channels: u16) -> AudioFormat {
        let bytes = match sample {
            SampleFmt::S16 => 2,
            SampleFmt::F32 => 4,
        };
        AudioFormat {
            rate: 48_000,
            channels,
            sample,
            block_align: bytes * channels as usize,
        }
    }

    // ---------------------------------------------------------- 锚点换算

    #[test]
    fn media_time_is_zero_at_the_anchor() {
        // 刚 Start() 完就采样：t0 = 0，p = p0
        assert_eq!(media_time_us(0, 48_000, 48_000, 48_000), 0);
    }

    #[test]
    fn media_time_counts_one_microsecond_per_tick_at_one_mhz() {
        // GetFrequency 报的通常就是 1 MHz 上下，正好一步一微秒
        assert_eq!(media_time_us(0, 0, 1_000, 1_000_000), 1_000);
    }

    #[test]
    fn media_time_advances_one_second_per_48000_samples() {
        // V1 的泵是按 48 kHz 的采样步进喂 PCM 的，时钟也该按这个走
        assert_eq!(media_time_us(0, 0, 48_000, 48_000), 1_000_000);
        assert_eq!(media_time_us(0, 0, 24_000, 48_000), 500_000);
        assert_eq!(media_time_us(0, 0, 1, 48_000), 20);
    }

    #[test]
    fn media_time_carries_on_from_where_a_pause_left_it() {
        // 暂停时把当时的 local_us 冻起来，恢复时它就是新的 t0，
        // P0 重新采样。两边接上，时间不能往回跳。
        let frozen = media_time_us(0, 0, 48_000, 48_000);
        assert_eq!(frozen, 1_000_000);

        assert_eq!(media_time_us(frozen, 96_000, 96_000, 48_000), 1_000_000);
        assert_eq!(media_time_us(frozen, 96_000, 144_000, 48_000), 2_000_000);
    }

    #[test]
    fn media_time_never_goes_negative_when_the_clock_lags_the_anchor() {
        // 锚点采下去之后设备偶尔往回挪一格：-10 个采样约合 -208 微秒
        assert_eq!(media_time_us(0, 1_000, 990, 48_000), 0);
        assert_eq!(media_time_us(500, 1_000, 990, 48_000), 292);

        // 差得更多的时候。这里是"减法必须在 i128 上做"的用例：
        // p0 比 p 大 100 万，两个 u64 直接相减会绕成 1.8e19，
        // 再乘 1e6 就彻底是个垃圾数，而正确答案是 30 秒减掉 20.833 秒。
        assert_eq!(media_time_us(30_000_000, 1_000_000, 0, 48_000), 9_166_667);
    }

    #[test]
    fn media_time_holds_still_when_the_clock_reports_no_frequency() {
        // freq 正常不可能是 0。真拿到 0 就当钟停了，别去除它。
        assert_eq!(media_time_us(1_234, 0, 999_999, 0), 1_234);
        assert_eq!(media_time_us(-5, 0, 999_999, 0), 0);
    }

    #[test]
    fn media_time_stays_exact_over_a_long_session() {
        // 四小时，48 kHz。采样数乘上 1e6 还在 i64 里，但不能有舍入漂移。
        let p = 48_000u64 * 4 * 3600;
        assert_eq!(media_time_us(0, 0, p, 48_000), 4 * 3600 * 1_000_000);
    }

    // ---------------------------------------------------------- 格式

    #[test]
    fn audio_format_reports_the_byte_rate_the_pump_needs() {
        let s16 = fmt(SampleFmt::S16, 2);
        let f32 = fmt(SampleFmt::F32, 2);

        assert_eq!(s16.bytes_per_sec(), 192_000.0);
        assert_eq!(f32.bytes_per_sec(), 384_000.0);
        assert_eq!(s16.block_align, 4);
        assert_eq!(f32.block_align, 8);
    }

    #[test]
    fn audio_format_knows_whether_it_is_the_planned_one() {
        // 计划里写死的是 48000 / s16 / 立体声
        assert!(fmt(SampleFmt::S16, 2).is_plan());
        // 任何一个不对，就是走了 IsFormatSupported 的回退
        assert!(!fmt(SampleFmt::F32, 2).is_plan());
        assert!(!fmt(SampleFmt::S16, 1).is_plan());
        assert!(!AudioFormat {
            rate: 44_100,
            channels: 2,
            sample: SampleFmt::S16,
            block_align: 4,
        }
        .is_plan());
    }

    #[test]
    fn audio_format_hands_ffmpeg_the_names_it_understands() {
        assert_eq!(fmt(SampleFmt::S16, 2).ffmpeg_sample(), "s16");
        assert_eq!(fmt(SampleFmt::F32, 2).ffmpeg_sample(), "f32");
        // ffmpeg 的 sample_fmt 就这两个名字，别拼出 "s16le" 之类
        assert!(!fmt(SampleFmt::S16, 2).ffmpeg_sample().contains("le"));
    }

    // ---------------------------------------------------------- 音量

    #[test]
    fn unity_gain_leaves_the_samples_untouched() {
        let mut pcm: [i16; 4] = [-32768, 32767, 1000, -1000];
        unsafe { apply_gain(pcm.as_mut_ptr() as *mut u8, 2, fmt(SampleFmt::S16, 2), 1.0) };
        assert_eq!(pcm, [-32768, 32767, 1000, -1000]);
    }

    #[test]
    fn mute_writes_silence() {
        let mut pcm: [i16; 4] = [-32768, 32767, 1000, -1000];
        unsafe { apply_gain(pcm.as_mut_ptr() as *mut u8, 2, fmt(SampleFmt::S16, 2), 0.0) };
        assert_eq!(pcm, [0, 0, 0, 0]);
    }

    #[test]
    fn s16_gain_clamps_instead_of_wrapping() {
        // 拉到 2 倍，两头都溢出。绕回去就是一串爆音，必须夹住。
        let mut pcm: [i16; 4] = [-32768, 32767, 1000, -1000];
        unsafe { apply_gain(pcm.as_mut_ptr() as *mut u8, 2, fmt(SampleFmt::S16, 2), 2.0) };
        assert_eq!(pcm, [-32768, 32767, 2000, -2000]);
    }

    #[test]
    fn s16_gain_uses_the_same_step_on_every_channel() {
        // 一帧两个采样，左右声道要按同一个比例走，不然声像会偏
        let mut pcm: [i16; 2] = [1000, 2000];
        unsafe { apply_gain(pcm.as_mut_ptr() as *mut u8, 1, fmt(SampleFmt::S16, 2), 0.5) };
        assert_eq!(pcm, [500, 1000]);
    }

    #[test]
    fn f32_gain_is_plain_multiplication() {
        // 浮点格式不做钳位，超过 ±1 就这么交给设备
        let mut pcm: [f32; 3] = [0.5, -0.5, 0.75];
        unsafe { apply_gain(pcm.as_mut_ptr() as *mut u8, 3, fmt(SampleFmt::F32, 1), 2.0) };
        assert_eq!(pcm, [1.0, -1.0, 1.5]);
    }

    #[test]
    fn apply_gain_does_nothing_to_zero_frames() {
        let mut pcm: [i16; 2] = [7, 9];
        unsafe { apply_gain(pcm.as_mut_ptr() as *mut u8, 0, fmt(SampleFmt::S16, 2), 0.0) };
        assert_eq!(pcm, [7, 9]);
    }
}
