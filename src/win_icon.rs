//! 运行时窗口图标：把 exe 资源里嵌入的应用图标设为顶层窗口的图标。
//!
//! 背景：Slint/winit 创建窗口时不设 window icon，Windows 原生标题栏就回退到
//! 窗口类的默认图标（IDI_APPLICATION，即那个“默认应用”图标），并不会自动采用
//! exe 资源里的图标。需要在窗口创建后以 `WM_SETICON` 显式把资源图标发上去，
//! 这样标题栏小图标、任务栏/Alt-Tab 大图标都会变成我们的场记板图标。
//!
//! 图标句柄由 `LoadImage` 创建，窗口在其存活期间持续引用，因此**有意不释放**
//! （随进程结束由系统回收）。

use windows_sys::core::PCWSTR;
use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, ICON_BIG, ICON_SMALL, IMAGE_ICON, LoadImageW, SendMessageW, SM_CXICON,
    SM_CXSMICON, SM_CYICON, SM_CYSMICON, WM_SETICON,
};

/// 应用图标资源组 ID，与 assets/app_icon.rc 的 `1 ICON "app_icon.ico"` 保持一致。
const APP_ICON_RES_ID: usize = 1;

/// 把 exe 内嵌的应用图标设为 hwnd 的窗口/任务栏图标（大小各一枚）。
pub fn apply_window_icon(hwnd: HWND) {
    if hwnd.is_null() {
        return;
    }
    let hinst: HINSTANCE = unsafe { GetModuleHandleW(std::ptr::null()) };
    if hinst.is_null() {
        return;
    }
    // MAKEINTRESOURCE(1)：低字是资源 ID，高字为 0，表示按整数 ID 而非名称查找。
    let name: PCWSTR = APP_ICON_RES_ID as PCWSTR;

    // 小图标给标题栏、大图标给任务栏/Alt-Tab；尺寸按系统 DPI 缩放后的度量取，
    // LoadImage 会在图标帧里挑最接近的一档（我们的 .ico 含 16/20/24/32/40/48/…）。
    let small_cx = unsafe { GetSystemMetrics(SM_CXSMICON) };
    let small_cy = unsafe { GetSystemMetrics(SM_CYSMICON) };
    let big_cx = unsafe { GetSystemMetrics(SM_CXICON) };
    let big_cy = unsafe { GetSystemMetrics(SM_CYICON) };

    let load = |cx: i32, cy: i32| unsafe { LoadImageW(hinst, name, IMAGE_ICON, cx, cy, 0) };
    let send = |which: u32, icon: isize| {
        unsafe { SendMessageW(hwnd, WM_SETICON, which as WPARAM, icon as LPARAM) };
    };

    let small = load(small_cx, small_cy);
    if !small.is_null() {
        send(ICON_SMALL, small as isize);
    }
    let big = load(big_cx, big_cy);
    if !big.is_null() {
        send(ICON_BIG, big as isize);
    }
}

/// 从 Slint 顶层窗口取得 winit 原生 HWND（Win32 raw window handle）。
/// 拿不到（尚未创建 / 非 Windows 窗口）时返回空指针，调用方自行兜底。
pub fn main_window_hwnd(window: &slint::Window) -> HWND {
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
