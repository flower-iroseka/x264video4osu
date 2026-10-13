//! 视频预览：用 libmpv 在主窗口里嵌一块播放画面。
//!
//! ## 为什么是 libmpv + 子窗口
//!
//! 主窗口是 Slint 的 femtovg(OpenGL) 渲染的，想让画面进去有两条路：把 mpv 的渲染
//! 塞进已有的 GL 上下文（render API），或者让 mpv 自己开一个原生子窗口。选了后者 ——
//! femtovg 在本项目里是 vendored 且打过 DPI 补丁的，共用上下文的耦合太深。
//!
//! 做过验证（`examples/preview_spike.rs`）：在窗口客户区里建一个子窗口，把句柄用
//! `wid` 交给 mpv，画面、声音、Slint 界面三者都正常。
//!
//! ## 线程模型
//!
//! mpv 的全部交互都跑在**一条工作线程**上，UI 线程不直接碰 mpv：
//! UI 通过 [`Preview::open`] 等方法把命令丢进通道，每 100ms 从 [`Preview::state`]
//! 读一次最新状态（和 `FfmpegService` 那套「后台线程 + UI 轮询」保持一致）。
//!
//! 这么做还有个好处：`mpv_handle` 的裸指针从来不需要跨线程传递。
//!
//! ## DLL
//!
//! `tools/libmpv-2.dll`，运行时用 `libloading` 动态加载，不引入编译期链接依赖。
//! 找不到就是预览不可用（[`Preview::is_available`] 返回 false），程序其余部分照常。

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows_sys::Win32::Foundation::HWND;

/// 随程序分发的 libmpv 文件名。
///
/// 注意是 `libmpv-2.dll` 而不是 `mpv-2.dll` —— dev 包里就是这个名字。
///
/// 分发的是 LGPL 构建（zhongfly/mpv-winbuild 的 `mpv-dev-lgpl-x86_64-*.7z`，
/// 构建参数里带 `-Dgpl=false`），不是 mpv 默认的 GPL 构建。本项目按 MIT 发布，
/// 用 GPL 版的 libmpv 会要求整个程序跟着变 GPL；LGPL 版没这个约束，换 DLL 的时候
/// 别拿错了。
const LIBMPV_FILE: &str = "libmpv-2.dll";

// ---- libmpv 的常量和结构（对着 include/mpv/client.h 抄） ----

const MPV_FORMAT_FLAG: c_int = 3;
const MPV_FORMAT_DOUBLE: c_int = 5;

const MPV_EVENT_NONE: c_int = 0;
const MPV_EVENT_SHUTDOWN: c_int = 1;

/// `mpv_event`。注意 `event_id` 在 C 里是枚举（int），不是 64 位。
#[repr(C)]
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

/// 从 DLL 里解析出来的入口。签名逐个对着 `client.h` 核过。
struct MpvFns {
    create: unsafe extern "C" fn() -> *mut c_void,
    initialize: unsafe extern "C" fn(*mut c_void) -> c_int,
    terminate_destroy: unsafe extern "C" fn(*mut c_void),
    set_option_string: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    command: unsafe extern "C" fn(*mut c_void, *const *const c_char) -> c_int,
    set_property: unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut c_void) -> c_int,
    get_property: unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut c_void) -> c_int,
    wait_event: unsafe extern "C" fn(*mut c_void, f64) -> *mut MpvEvent,
    error_string: unsafe extern "C" fn(c_int) -> *const c_char,
}

/// 音量上限。mpv 默认允许到 130（当放大器用），这里收在 100，
/// 滑块 0..100 才和实际音量一一对应。
pub const VOLUME_MAX: f64 = 100.0;

/// 预览面板要显示的状态。UI 每 100ms 取一次。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PreviewState {
    /// 已经装进一个文件（mpv 不在 idle）
    pub loaded: bool,
    pub paused: bool,
    /// 当前播放位置（秒）；没加载文件时为 0
    pub time_pos: f64,
    /// 总时长（秒）；未知时为 0
    pub duration: f64,
    /// 音量 0..=[`VOLUME_MAX`]
    pub volume: f64,
    pub mute: bool,
}

impl Default for PreviewState {
    fn default() -> Self {
        // 不能 derive：derive 出来的 volume 是 0（静音），而 mpv 的初始音量是 100。
        // 状态还没读到之前界面会先按这个默认值画，用 0 的话滑块会先闪到最左边。
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

/// UI 发给预览线程的命令。
enum Command {
    Open(PathBuf),
    SetPaused(bool),
    /// 播放/暂停取反，由工作线程读实时状态决定方向。
    TogglePlay,
    Seek(f64),
    /// 相对当前位置跳，单位秒，可负。
    SeekBy(f64),
    /// 正数前进、负数后退，单位是帧。逐帧步进用 ±1。
    StepFrames(i64),
    /// 音量，0..=[`VOLUME_MAX`]
    SetVolume(f64),
    /// 在当前音量上加减，由工作线程读实时值。快捷键用，理由同 [`Command::ToggleMute`]。
    StepVolume(f64),
    /// 静音取反，同样由工作线程读实时状态。
    ToggleMute,
}

/// 预览会话。持有它就代表预览开着；`Drop` 会停掉工作线程并销毁 mpv。
pub struct Preview {
    cmd_tx: Sender<Command>,
    state: Arc<Mutex<PreviewState>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// `loadfile` 之后 mpv 是异步加载的，这里记一下最近一次请求，
    /// 免得在真正加载完成前就把 `open` 当成失败。
    _last_path: PathBuf,
}

/// libmpv 在本机是否可用（DLL 在不在）。
pub fn is_available() -> bool {
    libmpv_path().exists()
}

fn libmpv_path() -> PathBuf {
    crate::services::ffmpeg_config::tool_path(LIBMPV_FILE)
}

impl Preview {
    /// 起一个预览会话，把画面挂到 `wid` 这个子窗口上。
    ///
    /// 内部要等 mpv 初始化完才返回，所以是阻塞的（正常几毫秒）。失败时返回错误描述，
    /// 调用方据此把预览入口禁掉。
    pub fn new(wid: HWND) -> Result<Self, String> {
        Self::start(wid, &[])
    }

    /// `extra_options` 是给测试用的补充选项（比如关掉音频），正常调用传空。
    fn start(wid: HWND, extra_options: &'static [(&'static str, &'static str)]) -> Result<Self, String> {
        let (cmd_tx, cmd_rx) = channel::<Command>();
        let (ready_tx, ready_rx) = channel::<Result<(), String>>();
        let state = Arc::new(Mutex::new(PreviewState::default()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let state = state.clone();
            let stop = stop.clone();
            // HWND 是裸指针，不是 Send；转成 isize 传进去再转回来。
            let wid = wid as isize;
            std::thread::Builder::new()
                .name("mpv-preview".into())
                .spawn(move || {
                    worker(wid as HWND, extra_options, cmd_rx, ready_tx, state, stop);
                })
                .map_err(|e| format!("创建预览线程失败：{e}"))?
        };

        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(Self {
                cmd_tx,
                state,
                stop,
                thread: Some(thread),
                _last_path: PathBuf::new(),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("预览线程启动超时".into()),
        }
    }

    /// 当前状态快照。
    pub fn state(&self) -> PreviewState {
        *self.state.lock().unwrap()
    }

    /// 换一个文件。同样是异步的，真正加载完看 [`PreviewState::loaded`]。
    pub fn open(&self, path: &Path) {
        let _ = self.cmd_tx.send(Command::Open(path.to_path_buf()));
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.cmd_tx.send(Command::SetPaused(paused));
    }

    /// 播放/暂停取反。
    ///
    /// 不用 `set_paused(!state().paused)`：`state()` 是 25ms 轮询出来的快照，
    /// 长按空格时的自动重复会连着发好几条命令，拿旧快照算就会翻错。
    /// 交给握着 mpv 的那个线程读实时值。
    pub fn toggle_paused(&self) {
        let _ = self.cmd_tx.send(Command::TogglePlay);
    }

    /// 跳到绝对位置（秒）。精确 seek，用在进度条松手之后。
    pub fn seek(&self, secs: f64) {
        if secs.is_finite() && secs >= 0.0 {
            let _ = self.cmd_tx.send(Command::Seek(secs));
        }
    }

    /// 相对当前位置跳（秒，可负）。快捷键用，两端由 mpv 自己夹住。
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
    /// 和 [`Preview::toggle_paused`] 一样交给工作线程算：`state()` 是 25ms 轮询的
    /// 快照，连按 ↑ 时几条命令都拿同一份旧值加 5，结果只涨了一档。
    pub fn step_volume(&self, delta: f64) {
        if delta.is_finite() && delta != 0.0 {
            let _ = self.cmd_tx.send(Command::StepVolume(delta));
        }
    }

    /// 静音取反。和 [`Preview::toggle_paused`] 一样，交给工作线程读实时值，
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

/// 预览线程：加载 DLL、建 mpv、装到子窗口上、然后跑命令/事件循环。
fn worker(
    wid: HWND,
    extra_options: &'static [(&'static str, &'static str)],
    cmd_rx: Receiver<Command>,
    ready_tx: Sender<Result<(), String>>,
    state: Arc<Mutex<PreviewState>>,
    stop: Arc<AtomicBool>,
) {
    let dll = libmpv_path();
    let lib = match unsafe { libloading::Library::new(&dll) } {
        Ok(l) => l,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("加载 {} 失败：{e}", dll.display())));
            return;
        }
    };

    let fns = match load_fns(&lib) {
        Ok(f) => f,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    let handle = unsafe { (fns.create)() };
    if handle.is_null() {
        let _ = ready_tx.send(Err("mpv_create 返回了空指针".into()));
        return;
    }

    // 先挂窗口再初始化 —— wid 只在 initialize 之前设才有效。
    if let Err(e) = configure(&fns, handle, wid, extra_options) {
        unsafe { (fns.terminate_destroy)(handle) };
        let _ = ready_tx.send(Err(e));
        return;
    }

    let _ = ready_tx.send(Ok(()));

    run_loop(&fns, handle, cmd_rx, state, stop);

    unsafe { (fns.terminate_destroy)(handle) };
}

/// 解析需要的入口。任何一个缺失都当成 DLL 不可用。
fn load_fns(lib: &libloading::Library) -> Result<MpvFns, String> {
    macro_rules! sym {
        ($name:literal, $ty:ty) => {
            unsafe {
                *lib.get::<$ty>(concat!($name, "\0").as_bytes())
                    .map_err(|e| format!("{} 里找不到 {}：{e}", LIBMPV_FILE, $name))?
            }
        };
    }

    Ok(MpvFns {
        create: sym!("mpv_create", unsafe extern "C" fn() -> *mut c_void),
        initialize: sym!("mpv_initialize", unsafe extern "C" fn(*mut c_void) -> c_int),
        terminate_destroy: sym!("mpv_terminate_destroy", unsafe extern "C" fn(*mut c_void)),
        set_option_string: sym!(
            "mpv_set_option_string",
            unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int
        ),
        command: sym!(
            "mpv_command",
            unsafe extern "C" fn(*mut c_void, *const *const c_char) -> c_int
        ),
        set_property: sym!(
            "mpv_set_property",
            unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut c_void) -> c_int
        ),
        get_property: sym!(
            "mpv_get_property",
            unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut c_void) -> c_int
        ),
        wait_event: sym!(
            "mpv_wait_event",
            unsafe extern "C" fn(*mut c_void, f64) -> *mut MpvEvent
        ),
        error_string: sym!("mpv_error_string", unsafe extern "C" fn(c_int) -> *const c_char),
    })
}

fn err_text(fns: &MpvFns, code: c_int) -> String {
    let p = unsafe { (fns.error_string)(code) };
    if p.is_null() {
        format!("error {code}")
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// 设选项 + 初始化。
fn configure(
    fns: &MpvFns,
    handle: *mut c_void,
    wid: HWND,
    extra_options: &'static [(&'static str, &'static str)],
) -> Result<(), String> {
    let wid_value = CString::new((wid as usize).to_string()).unwrap();
    let mut options: Vec<(&str, &CStr)> = vec![
        ("wid", wid_value.as_c_str()),
        // 播到结尾停在最后一帧，不要自己退出
        ("keep-open", c"yes"),
        // 没有文件时也不要退出，方便反复换文件
        ("idle", c"yes"),
        // 预览窗口不需要键盘/鼠标交互，也不需要 mpv 自带的控制条
        ("input-default-bindings", c"no"),
        ("input-vo-keyboard", c"no"),
        ("osc", c"no"),
        ("osd-level", c"0"),
        // 不要往控制台打日志
        ("terminal", c"no"),
        ("msg-level", c"all=no"),
        ("audio-display", c"no"),
        // 音量滑块只管 0..100；mpv 默认能到 130（当放大器用），这里收掉
        ("volume-max", c"100"),
    ];

    // 测试时补进来的选项（关音频之类）得在 initialize 之前一起设
    let extra_cstrings: Vec<(CString, CString)> = extra_options
        .iter()
        .map(|(k, v)| (CString::new(*k).unwrap(), CString::new(*v).unwrap()))
        .collect();
    for (k, v) in &extra_cstrings {
        options.push((k.to_str().unwrap(), v.as_c_str()));
    }

    for (name, value) in options {
        let n = CString::new(name).unwrap();
        let rc = unsafe { (fns.set_option_string)(handle, n.as_ptr(), value.as_ptr()) };
        if rc < 0 {
            return Err(format!("mpv 选项 {name} 设置失败：{}", err_text(fns, rc)));
        }
    }

    let rc = unsafe { (fns.initialize)(handle) };
    if rc < 0 {
        return Err(format!("mpv_initialize 失败：{}", err_text(fns, rc)));
    }
    Ok(())
}

/// 跑一条 mpv 命令。用参数数组而不是命令字符串 —— 命令字符串会解析转义，
/// Windows 路径里的反斜杠会被吃掉。
fn command(fns: &MpvFns, handle: *mut c_void, args: &[&CStr]) -> c_int {
    let mut ptrs: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe { (fns.command)(handle, ptrs.as_ptr()) }
}

fn set_flag(fns: &MpvFns, handle: *mut c_void, name: &str, value: bool) -> c_int {
    let n = CString::new(name).unwrap();
    let mut v: c_int = i32::from(value);
    unsafe {
        (fns.set_property)(
            handle,
            n.as_ptr(),
            MPV_FORMAT_FLAG,
            &mut v as *mut c_int as *mut c_void,
        )
    }
}

fn set_double(fns: &MpvFns, handle: *mut c_void, name: &str, value: f64) -> c_int {
    let n = CString::new(name).unwrap();
    let mut v = value;
    unsafe {
        (fns.set_property)(
            handle,
            n.as_ptr(),
            MPV_FORMAT_DOUBLE,
            &mut v as *mut f64 as *mut c_void,
        )
    }
}

fn get_flag(fns: &MpvFns, handle: *mut c_void, name: &str) -> Option<bool> {
    let n = CString::new(name).ok()?;
    let mut v: c_int = 0;
    let rc = unsafe {
        (fns.get_property)(
            handle,
            n.as_ptr(),
            MPV_FORMAT_FLAG,
            &mut v as *mut c_int as *mut c_void,
        )
    };
    (rc >= 0).then_some(v != 0)
}

fn get_double(fns: &MpvFns, handle: *mut c_void, name: &str) -> Option<f64> {
    let n = CString::new(name).ok()?;
    let mut v: f64 = 0.0;
    let rc = unsafe {
        (fns.get_property)(
            handle,
            n.as_ptr(),
            MPV_FORMAT_DOUBLE,
            &mut v as *mut f64 as *mut c_void,
        )
    };
    (rc >= 0 && v.is_finite()).then_some(v)
}

/// 主循环：处理命令 → 排空 mpv 事件 → 刷新状态缓存。
///
/// 事件必须排空，不然 mpv 内部的事件队列会一直涨，最后把自己的播放线程卡住。
fn run_loop(
    fns: &MpvFns,
    handle: *mut c_void,
    cmd_rx: Receiver<Command>,
    state: Arc<Mutex<PreviewState>>,
    stop: Arc<AtomicBool>,
) {
    const POLL: Duration = Duration::from_millis(25);
    let mut shutdown = false;

    while !stop.load(Ordering::SeqCst) && !shutdown {
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => apply_command(fns, handle, cmd),
                // 断开说明命令发送端没了，也就是 `Preview` 已经析构、或者压根没建起来
                // （启动超时那条路 `stop` 永远不会被置位）。再转下去就是空转，收工。
                Err(TryRecvError::Disconnected) => {
                    shutdown = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }

        // 非阻塞地把积压的事件收掉
        loop {
            let ev = unsafe { (fns.wait_event)(handle, 0.0) };
            if ev.is_null() {
                break;
            }
            let id = unsafe { (*ev).event_id };
            if id == MPV_EVENT_NONE {
                break;
            }
            if id == MPV_EVENT_SHUTDOWN {
                shutdown = true;
                break;
            }
        }

        refresh_state(fns, handle, &state);
        std::thread::sleep(POLL);
    }
}

fn apply_command(fns: &MpvFns, handle: *mut c_void, cmd: Command) {
    match cmd {
        Command::Open(path) => {
            let verb = CString::new("loadfile").unwrap();
            // mpv 在 Windows 上接受 UTF-8 路径
            let Ok(p) = CString::new(path.to_string_lossy().as_bytes()) else {
                return;
            };
            command(fns, handle, &[verb.as_c_str(), p.as_c_str()]);
        }
        Command::SetPaused(paused) => {
            set_flag(fns, handle, "pause", paused);
        }
        Command::TogglePlay => {
            let paused = get_flag(fns, handle, "pause").unwrap_or(true);
            set_flag(fns, handle, "pause", !paused);
        }
        Command::Seek(secs) => {
            let verb = CString::new("seek").unwrap();
            let value = CString::new(format!("{secs:.3}")).unwrap();
            // absolute+exact：精确到帧，裁剪打点用得上，代价是慢一点
            let mode = CString::new("absolute+exact").unwrap();
            command(fns, handle, &[verb.as_c_str(), value.as_c_str(), mode.as_c_str()]);
        }
        Command::SeekBy(secs) => {
            let verb = CString::new("seek").unwrap();
            // 带符号的数字加 relative：让 mpv 按实时位置算，别拿我们手上那份
            // 最多旧 25ms 的快照去加，长按连发会越走越偏
            let value = CString::new(format!("{secs:+.3}")).unwrap();
            let mode = CString::new("relative+exact").unwrap();
            command(fns, handle, &[verb.as_c_str(), value.as_c_str(), mode.as_c_str()]);
        }
        Command::StepFrames(n) => {
            let verb = if n > 0 {
                CString::new("frame-step").unwrap()
            } else {
                CString::new("frame-back-step").unwrap()
            };
            for _ in 0..n.unsigned_abs() {
                command(fns, handle, &[verb.as_c_str()]);
            }
        }
        Command::SetVolume(volume) => {
            set_double(fns, handle, "volume", volume.clamp(0.0, VOLUME_MAX));
        }
        Command::StepVolume(delta) => {
            let volume = get_double(fns, handle, "volume").unwrap_or(VOLUME_MAX);
            set_double(fns, handle, "volume", (volume + delta).clamp(0.0, VOLUME_MAX));
        }
        Command::ToggleMute => {
            let mute = get_flag(fns, handle, "mute").unwrap_or(false);
            set_flag(fns, handle, "mute", !mute);
        }
    }
}

fn refresh_state(fns: &MpvFns, handle: *mut c_void, state: &Arc<Mutex<PreviewState>>) {
    // idle-active 为 false 表示有文件在播；duration 拿不到时按 0 处理
    let loaded = get_flag(fns, handle, "idle-active").map(|idle| !idle).unwrap_or(false);
    let paused = get_flag(fns, handle, "pause").unwrap_or(true);
    let time_pos = get_double(fns, handle, "time-pos").unwrap_or(0.0);
    let duration = get_double(fns, handle, "duration").unwrap_or(0.0);
    let volume = get_double(fns, handle, "volume").unwrap_or(VOLUME_MAX);
    let mute = get_flag(fns, handle, "mute").unwrap_or(false);

    let mut s = state.lock().unwrap();
    let next = PreviewState {
        loaded,
        paused,
        time_pos: if loaded { time_pos } else { 0.0 },
        duration,
        volume,
        mute,
    };
    if *s != next {
        *s = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WS_POPUP, WS_VISIBLE,
    };

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

        // 越界要夹住（mpv 报的 time-pos 偶尔会略微超过 duration）
        let s = PreviewState {
            time_pos: 11.0,
            duration: 10.0,
            ..Default::default()
        };
        assert_eq!(s.progress(), 1.0);
    }

    #[test]
    fn volume_defaults_to_full_not_zero() {
        // mpv 起来时音量就是 100，默认值得跟着走，不然滑块会先闪到最左边
        let s = PreviewState::default();
        assert_eq!(s.volume, VOLUME_MAX);
        assert!(!s.mute);
    }

    #[test]
    fn libmpv_file_name_is_the_real_one() {
        // dev 包里的名字是 libmpv-2.dll，写成 mpv-2.dll 会一直加载失败
        assert_eq!(LIBMPV_FILE, "libmpv-2.dll");
        let p = libmpv_path();
        assert!(p.ends_with(LIBMPV_FILE), "路径结尾不对: {}", p.display());
    }

    /// 建一个摆在屏幕外的宿主窗口。测试不给人看，但窗口得是"可见"的，
    /// 否则 mpv 的视频输出可能不肯配置起来。
    fn offscreen_host() -> HWND {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let title: Vec<u16> = "\0".encode_utf16().collect();
        unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP | WS_VISIBLE,
                -4000,
                -4000,
                320,
                180,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null_mut(),
            )
        }
    }

    /// 用项目自带的 ffmpeg 造一段 10 秒、带音轨的测试片。
    fn make_clip() -> Option<PathBuf> {
        let ffmpeg = crate::services::ffmpeg_config::ffmpeg_path();
        if !ffmpeg.exists() {
            return None;
        }
        let path = crate::testutil::scratch_path("preview_itest_clip.mp4");
        let _ = std::fs::remove_file(&path);
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-y", "-v", "error",
                "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=30:duration=10",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=10",
                "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-shortest",
            ])
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        status.success().then_some(path)
    }

    /// 轮询等状态满足条件；超时返回最后一次的快照。
    fn wait_until(p: &Preview, timeout: Duration, f: impl Fn(&PreviewState) -> bool) -> PreviewState {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let s = p.state();
            if f(&s) {
                return s;
            }
            if std::time::Instant::now() >= deadline {
                return s;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 走一遍真实的播放控制。需要 tools/libmpv-2.dll，缺了就跳过。
    #[test]
    fn plays_pauses_seeks_and_steps_frames() {
        if !is_available() {
            eprintln!("tools/{LIBMPV_FILE} 不在，跳过预览集成测试");
            return;
        }
        let Some(clip) = make_clip() else {
            eprintln!("没有可用的 ffmpeg，跳过预览集成测试");
            return;
        };

        let host = offscreen_host();
        assert!(!host.is_null(), "建不出宿主窗口");

        // 关掉音频：跑测试不该出声
        let preview = Preview::start(host, &[("audio", "no")]).expect("起预览失败");

        // 还没加载任何东西
        assert!(!preview.state().loaded, "刚起来不该是已加载状态");

        preview.open(&clip);
        let s = wait_until(&preview, Duration::from_secs(20), |s| s.loaded && s.duration > 0.0);
        assert!(s.loaded, "文件没加载上");
        assert!(
            (s.duration - 10.0).abs() < 0.5,
            "时长读出来是 {}，应该接近 10",
            s.duration
        );

        // 定位
        preview.set_paused(true);
        preview.seek(5.0);
        let s = wait_until(&preview, Duration::from_secs(15), |s| (s.time_pos - 5.0).abs() < 0.2);
        assert!(
            (s.time_pos - 5.0).abs() < 0.2,
            "seek(5.0) 之后位置是 {}",
            s.time_pos
        );
        assert!(s.paused, "seek 之前已经让它暂停了");

        // 逐帧前进一帧（30fps → 1/30 ≈ 0.0333 秒）
        let before = preview.state().time_pos;
        preview.step_frames(1);
        let s = wait_until(&preview, Duration::from_secs(10), |s| s.time_pos > before + 0.01);
        let advance = s.time_pos - before;
        assert!(
            (0.02..0.06).contains(&advance),
            "前进一帧走了 {advance:.4}s，30fps 下应该约 0.0333"
        );

        // 逐帧后退一帧，应当回到原处
        preview.step_frames(-1);
        let s = wait_until(&preview, Duration::from_secs(10), |s| (s.time_pos - before).abs() < 0.02);
        assert!(
            (s.time_pos - before).abs() < 0.02,
            "后退一帧之后是 {}，应该回到 {before}",
            s.time_pos
        );

        // 相对跳转：±2 秒。走的是 mpv 的 relative，不是我们拿快照加一加
        preview.seek_by(2.0);
        let s = wait_until(&preview, Duration::from_secs(10), |s| s.time_pos > before + 1.5);
        assert!(
            (s.time_pos - (before + 2.0)).abs() < 0.25,
            "seek_by(+2.0) 之后是 {}，应该接近 {}",
            s.time_pos,
            before + 2.0
        );
        preview.seek_by(-2.0);
        let s = wait_until(&preview, Duration::from_secs(10), |s| (s.time_pos - before).abs() < 0.25);
        assert!(
            (s.time_pos - before).abs() < 0.25,
            "seek_by(-2.0) 之后是 {}，应该回到 {before}",
            s.time_pos
        );

        // 往回跳过头：mpv 会停在 0，不该跑到负数去
        preview.seek(1.0);
        let _ = wait_until(&preview, Duration::from_secs(10), |s| (s.time_pos - 1.0).abs() < 0.2);
        preview.seek_by(-30.0);
        let s = wait_until(&preview, Duration::from_secs(10), |s| s.time_pos < 0.2);
        assert!(s.time_pos >= 0.0, "往回跳过头跑到负数了：{}", s.time_pos);

        // 播放/暂停取反：连发两次回到原来的状态
        let was_paused = preview.state().paused;
        preview.toggle_paused();
        let s = wait_until(&preview, Duration::from_secs(10), |s| s.paused != was_paused);
        assert!(s.paused != was_paused, "toggle_paused 之后 pause 没跟着变");
        preview.toggle_paused();
        let s = wait_until(&preview, Duration::from_secs(10), |s| s.paused == was_paused);
        assert!(s.paused == was_paused, "再 toggle 一次没回到原来的 pause 状态");

        // 音量：属性名或格式写错的话，设下去读回来还是原来的值
        preview.set_volume(40.0);
        let s = wait_until(&preview, Duration::from_secs(10), |s| {
            (s.volume - 40.0).abs() < 0.6
        });
        assert!(
            (s.volume - 40.0).abs() < 0.6,
            "音量设成 40 之后读回来是 {}",
            s.volume
        );

        // 静音取反：从没静音开始，toggle 一次该静音，再一次该恢复
        assert!(!preview.state().mute, "这时候不该是静音");
        preview.toggle_mute();
        let s = wait_until(&preview, Duration::from_secs(10), |s| s.mute);
        assert!(s.mute, "toggle 之后没静音");
        preview.toggle_mute();
        let s = wait_until(&preview, Duration::from_secs(10), |s| !s.mute);
        assert!(!s.mute, "再 toggle 一次还是静音");

        // 超过上限要被夹住（volume-max 设成了 100）
        preview.set_volume(1000.0);
        let s = wait_until(&preview, Duration::from_secs(10), |s| s.volume <= VOLUME_MAX + 0.6);
        assert!(s.volume <= VOLUME_MAX + 0.6, "音量越过上限了：{}", s.volume);

        drop(preview);
        unsafe { DestroyWindow(host) };
        let _ = std::fs::remove_file(&clip);
    }
}
