//! 第 0 步验证：mpv 能不能嵌进 Slint（femtovg / OpenGL）窗口。
//!
//! 这是 3.1 视频预览功能的落地前提。主窗口是 OpenGL 渲染的，mpv 要嵌进来就得在
//! 客户区上叠一个原生子窗口（HWND）。子窗口盖在 OpenGL 渲染的父窗口上有可能闪烁、
//! 层级错乱或者干脆不显示 —— 这件事不成立，整个预览方案就得换路子。
//!
//! 这个例子故意做得很小，不碰 app.rs / win_dialog.rs 等现有代码：
//! 自己开一个 Slint 窗口 → 在它上面建一个子窗口 → 把子窗口句柄交给 libmpv →
//! 让它播一段带声音的视频。
//!
//! 跑法（先确认 tools/libmpv-2.dll 在）：
//!   cargo run --example preview_spike
//!
//! 程序会自己打印客观证据（视频输出有没有配上、分辨率、音频输出参数、
//! 两次时间点对比证明在往前走）。画面到底有没有显示出来，还需要人眼确认。

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::time::Duration;

use windows_sys::core::PCWSTR;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, IsWindowVisible, WS_CHILD, WS_CLIPSIBLINGS, WS_VISIBLE,
};

slint::slint! {
    export component SpikeWindow inherits Window {
        width: 640px;
        height: 400px;
        background: #303030;

        Text {
            x: 20px;
            y: 4px;
            text: "preview spike — 深色矩形里应该出现视频画面";
            color: #ffffff;
            font-size: 12px;
        }

        // 这块就是预览区的位置，mpv 的子窗口要正好压在上面。
        Rectangle {
            x: 20px;
            y: 24px;
            width: 600px;
            height: 340px;
            background: #101010;
        }
    }
}

/// libmpv 的句柄，对上层是不透明的。
type MpvHandle = c_void;

const MPV_FORMAT_FLAG: c_int = 3;
const MPV_FORMAT_INT64: c_int = 4;
const MPV_FORMAT_DOUBLE: c_int = 5;

/// 从 tools/libmpv-2.dll 里解析出来的几个函数。签名对着 include/mpv/client.h 抄。
struct Mpv {
    _lib: libloading::Library,
    handle: *mut MpvHandle,
    terminate_destroy: unsafe extern "C" fn(*mut MpvHandle),
    error_string: unsafe extern "C" fn(c_int) -> *const c_char,
    get_property: unsafe extern "C" fn(*mut MpvHandle, *const c_char, c_int, *mut c_void) -> c_int,
    get_property_string: unsafe extern "C" fn(*mut MpvHandle, *const c_char) -> *mut c_char,
    free: unsafe extern "C" fn(*mut c_void),
}

impl Mpv {
    fn err(&self, code: c_int) -> String {
        if code >= 0 {
            return "ok".into();
        }
        let p = unsafe { (self.error_string)(code) };
        if p.is_null() {
            format!("error {code}")
        } else {
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    }

    fn flag(&self, name: &str) -> Option<bool> {
        let n = CString::new(name).ok()?;
        let mut out: c_int = 0;
        let rc = unsafe {
            (self.get_property)(self.handle, n.as_ptr(), MPV_FORMAT_FLAG, &mut out as *mut _ as *mut c_void)
        };
        (rc >= 0).then_some(out != 0)
    }

    fn int64(&self, name: &str) -> Option<i64> {
        let n = CString::new(name).ok()?;
        let mut out: i64 = 0;
        let rc = unsafe {
            (self.get_property)(self.handle, n.as_ptr(), MPV_FORMAT_INT64, &mut out as *mut _ as *mut c_void)
        };
        (rc >= 0).then_some(out)
    }

    fn double(&self, name: &str) -> Option<f64> {
        let n = CString::new(name).ok()?;
        let mut out: f64 = 0.0;
        let rc = unsafe {
            (self.get_property)(self.handle, n.as_ptr(), MPV_FORMAT_DOUBLE, &mut out as *mut _ as *mut c_void)
        };
        (rc >= 0).then_some(out)
    }

    fn string(&self, name: &str) -> Option<String> {
        let n = CString::new(name).ok()?;
        let p = unsafe { (self.get_property_string)(self.handle, n.as_ptr()) };
        if p.is_null() {
            return None;
        }
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { (self.free)(p as *mut c_void) };
        Some(s)
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { (self.terminate_destroy)(self.handle) };
        }
    }
}

/// 加载 libmpv 并把窗口挂上去。任何一步失败都返回 Err，由调用方打印。
fn start_mpv(dll: &std::path::Path, wid: HWND, file: &std::path::Path) -> Result<Mpv, String> {
    type FnCreate = unsafe extern "C" fn() -> *mut MpvHandle;
    type FnInit = unsafe extern "C" fn(*mut MpvHandle) -> c_int;
    type FnTerminate = unsafe extern "C" fn(*mut MpvHandle);
    type FnSetOpt = unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int;
    type FnCmd = unsafe extern "C" fn(*mut MpvHandle, *const *const c_char) -> c_int;
    type FnErrStr = unsafe extern "C" fn(c_int) -> *const c_char;
    type FnGetProp = unsafe extern "C" fn(*mut MpvHandle, *const c_char, c_int, *mut c_void) -> c_int;
    type FnGetPropStr = unsafe extern "C" fn(*mut MpvHandle, *const c_char) -> *mut c_char;
    type FnFree = unsafe extern "C" fn(*mut c_void);

    let lib = unsafe { libloading::Library::new(dll) }
        .map_err(|e| format!("加载 {} 失败：{e}", dll.display()))?;

    unsafe {
        // 取成裸函数指针（Symbol 借用 Library，解引用成 fn 指针就独立了）
        let create = *lib
            .get::<FnCreate>(b"mpv_create\0")
            .map_err(|e| format!("找不到 mpv_create：{e}"))?;
        let initialize = *lib
            .get::<FnInit>(b"mpv_initialize\0")
            .map_err(|e| format!("找不到 mpv_initialize：{e}"))?;
        let terminate_destroy = *lib
            .get::<FnTerminate>(b"mpv_terminate_destroy\0")
            .map_err(|e| format!("找不到 mpv_terminate_destroy：{e}"))?;
        let set_option_string = *lib
            .get::<FnSetOpt>(b"mpv_set_option_string\0")
            .map_err(|e| format!("找不到 mpv_set_option_string：{e}"))?;
        let command = *lib
            .get::<FnCmd>(b"mpv_command\0")
            .map_err(|e| format!("找不到 mpv_command：{e}"))?;
        let error_string = *lib
            .get::<FnErrStr>(b"mpv_error_string\0")
            .map_err(|e| format!("找不到 mpv_error_string：{e}"))?;
        let get_property = *lib
            .get::<FnGetProp>(b"mpv_get_property\0")
            .map_err(|e| format!("找不到 mpv_get_property：{e}"))?;
        let get_property_string = *lib
            .get::<FnGetPropStr>(b"mpv_get_property_string\0")
            .map_err(|e| format!("找不到 mpv_get_property_string：{e}"))?;
        let free = *lib
            .get::<FnFree>(b"mpv_free\0")
            .map_err(|e| format!("找不到 mpv_free：{e}"))?;

        let handle = create();
        if handle.is_null() {
            return Err("mpv_create 返回了空指针".into());
        }

        let mv = Mpv {
            _lib: lib,
            handle,
            terminate_destroy,
            error_string,
            get_property,
            get_property_string,
            free,
        };

        // wid 必须在 mpv_initialize 之前设。
        let wid_str = CString::new((wid as usize).to_string()).unwrap();
        for (name, value) in [
            ("wid", wid_str.as_c_str()),
            ("keep-open", c"yes"),
            ("loop-file", c"inf"),
            ("idle", c"yes"),
            ("input-default-bindings", c"no"),
            ("input-vo-keyboard", c"no"),
            ("osc", c"no"),
            ("osd-level", c"0"),
            ("terminal", c"no"),
            ("msg-level", c"all=no"),
            ("audio-display", c"no"),
        ] {
            let n = CString::new(name).unwrap();
            let rc = set_option_string(mv.handle, n.as_ptr(), value.as_ptr());
            if rc < 0 {
                return Err(format!("设置选项 {name}={value:?} 失败：{}", mv.err(rc)));
            }
        }

        let rc = initialize(mv.handle);
        if rc < 0 {
            return Err(format!("mpv_initialize 失败：{}", mv.err(rc)));
        }

        // 用参数数组而不是命令字符串：mpv_command_string 会解析转义，
        // Windows 路径里的反斜杠会被吃掉（`\T` 之类），必须走数组形式。
        let verb = CString::new("loadfile").unwrap();
        let path = CString::new(file.to_string_lossy().as_bytes()).unwrap();
        let args: [*const c_char; 3] = [verb.as_ptr(), path.as_ptr(), std::ptr::null()];
        let rc = command(mv.handle, args.as_ptr());
        if rc < 0 {
            return Err(format!("loadfile 失败：{}", mv.err(rc)));
        }

        Ok(mv)
    }
}

/// 在主窗口客户区里建一个普通子窗口，用来当 mpv 的落脚点。
/// 用系统自带的 "STATIC" 类，省得自己注册窗口类。
unsafe fn create_host_child(parent: HWND, x: i32, y: i32, w: i32, h: i32) -> HWND {
    let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
    let title: Vec<u16> = "\0".encode_utf16().collect();
    let hinst = GetModuleHandleW(std::ptr::null());
    CreateWindowExW(
        0,
        class.as_ptr() as PCWSTR,
        title.as_ptr() as PCWSTR,
        WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
        x,
        y,
        w,
        h,
        parent,
        std::ptr::null_mut(),
        hinst,
        std::ptr::null_mut(),
    )
}

/// 问 mpv 一堆状态，用来客观判断「视频真的解出来了、音频输出真的起来了」。
fn diagnose(tag: &str, mv: &Mpv) {
    println!("--- 诊断 {tag} ---");
    match mv.flag("vo-configured") {
        Some(v) => println!("  vo-configured (视频输出已就绪) = {v}"),
        None => println!("  vo-configured = <读不到>"),
    }
    match (mv.int64("width"), mv.int64("height")) {
        (Some(w), Some(h)) => println!("  解码分辨率 = {w}x{h}"),
        _ => println!("  解码分辨率 = <读不到>"),
    }
    match mv.string("video-format") {
        Some(f) => println!("  像素格式 = {f}"),
        None => println!("  像素格式 = <读不到>"),
    }
    match mv.int64("audio-params/channel-count") {
        Some(c) => println!("  音频声道数 = {c}"),
        None => println!("  音频声道数 = <还没有>"),
    }
    match mv.string("audio-codec-name") {
        Some(c) => println!("  音频编码 = {c}"),
        None => println!("  音频编码 = <还没有>"),
    }
    match mv.double("time-pos") {
        Some(t) => println!("  播放位置 = {t:.3}s"),
        None => println!("  播放位置 = <还没有>"),
    }
    match mv.string("path") {
        Some(p) => println!("  已加载 = {p}"),
        None => println!("  已加载 = <没有>"),
    }
}

fn main() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dll = root.join("tools").join("libmpv-2.dll");
    let clip = root.join("tests-001").join("spike_clip.mp4");

    if !dll.exists() {
        eprintln!("[spike] 找不到 {},先把它放进 tools/", dll.display());
        std::process::exit(2);
    }
    if !clip.exists() {
        eprintln!("[spike] 找不到测试视频 {}", clip.display());
        std::process::exit(2);
    }

    let ui = SpikeWindow::new().expect("create SpikeWindow");

    // mpv 实例挂在外面，活到事件循环结束之后，这样能顺带验证退出时是否干净。
    let mpv_slot: std::rc::Rc<std::cell::RefCell<Option<Mpv>>> = Default::default();
    let child_slot: std::rc::Rc<std::cell::Cell<isize>> = Default::default();

    // winit 是异步建窗口的，show() 返回时窗口还不存在，得等一会儿再取 HWND。
    {
        let weak = ui.as_weak();
        let dll = dll.clone();
        let clip = clip.clone();
        let slot = mpv_slot.clone();
        let child_slot = child_slot.clone();
        slint::Timer::single_shot(Duration::from_millis(600), move || {
            let Some(ui) = weak.upgrade() else { return };
            let window = ui.window();
            let hwnd = hwnd_of(&window);
            if hwnd.is_null() {
                eprintln!("[spike] 拿不到主窗口 HWND —— 退出");
                let _ = slint::quit_event_loop();
                return;
            }
            println!("[spike] 主窗口 HWND = {hwnd:p}");

            // 预览区在 Slint 里是 (20,24) 600x340，换算成物理像素。
            let sf = window.scale_factor() as f32;
            let (x, y, w, h) = (
                (20.0 * sf) as i32,
                (24.0 * sf) as i32,
                (600.0 * sf) as i32,
                (340.0 * sf) as i32,
            );
            let child = unsafe { create_host_child(hwnd, x, y, w, h) };
            if child.is_null() {
                eprintln!("[spike] 创建子窗口失败");
                let _ = slint::quit_event_loop();
                return;
            }
            child_slot.set(child as isize);
            println!("[spike] 子窗口 HWND = {child:p}，位置 {x},{y} 尺寸 {w}x{h}");
            println!("[spike] 子窗口可见 = {}", unsafe { IsWindowVisible(child) } != 0);

            match start_mpv(&dll, child, &clip) {
                Ok(mv) => {
                    println!("[spike] mpv 已启动。下面是 mpv 自己的状态报告 ——");
                    *slot.borrow_mut() = Some(mv);
                }
                Err(e) => {
                    eprintln!("[spike] 启动 mpv 失败：{e}");
                    let _ = slint::quit_event_loop();
                }
            }
        });
    }

    // 四次采样：证明画面配上了、音频起来了、时间在往前走。
    for (delay, tag) in [
        (2500u64, "T+2.5s"),
        (8000, "T+8s"),
        (16000, "T+16s"),
        (24000, "T+24s"),
    ] {
        let slot = mpv_slot.clone();
        slint::Timer::single_shot(Duration::from_millis(delay), move || {
            if let Some(mv) = slot.borrow().as_ref() {
                diagnose(tag, mv);
            }
        });
    }

    slint::Timer::single_shot(Duration::from_secs(30), || {
        println!("[spike] 到时退出");
        let _ = slint::quit_event_loop();
    });

    ui.show().expect("show");
    slint::run_event_loop().expect("run_event_loop");

    // 先销毁子窗口，再销毁 mpv（Drop 里调 mpv_terminate_destroy），验证退出路径干净。
    let child = child_slot.get() as HWND;
    if !child.is_null() {
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(child) };
    }
    drop(mpv_slot);
    println!("[spike] mpv 已关闭，进程正常退出");
}

/// 从 Slint 窗口取 Win32 HWND（和 win_icon.rs 里同样的路子）。
fn hwnd_of(window: &slint::Window) -> HWND {
    use slint::winit_030::WinitWindowAccessor;
    window
        .with_winit_window(|w| {
            use slint::winit_030::winit::raw_window_handle::HasWindowHandle;
            match w.window_handle().map(|h| h.as_raw()) {
                Ok(slint::winit_030::winit::raw_window_handle::RawWindowHandle::Win32(win32)) => {
                    win32.hwnd.get() as HWND
                }
                _ => std::ptr::null_mut(),
            }
        })
        .unwrap_or(std::ptr::null_mut())
}
