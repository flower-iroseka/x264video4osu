//! mpv 落脚用的子窗口。
//!
//! mpv 需要一个原生窗口句柄（`wid` 选项）才知道往哪儿画。Slint 的窗口是它自己管的，
//! 我们不能直接把控件句柄交出去，所以在主窗口客户区里另外建一个普通子窗口垫在
//! 预览区那块矩形的位置上。
//!
//! 用系统自带的 `STATIC` 窗口类，省得注册自己的窗口类。mpv 会在这个子窗口里
//! 再建它自己的输出窗口，所以这里不需要处理绘制。
//!
//! 坐标一律是**物理像素、相对主窗口客户区**。调用方负责把 Slint 的逻辑坐标
//! 乘上缩放因子再传进来。

use windows_sys::core::PCWSTR;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, ShowWindow, SW_HIDE, SW_SHOWNA, WS_CHILD, WS_CLIPSIBLINGS,
};

/// 在主窗口 `parent` 下建一个子窗口，初始不可见（等摆好位置再显示，避免闪一下）。
/// 失败返回空指针。
pub fn create(parent: HWND) -> HWND {
    if parent.is_null() {
        return std::ptr::null_mut();
    }
    let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
    let title: Vec<u16> = "\0".encode_utf16().collect();
    unsafe {
        CreateWindowExW(
            0,
            class.as_ptr() as PCWSTR,
            title.as_ptr() as PCWSTR,
            // WS_CHILD 但不带 WS_VISIBLE：先藏着，摆好位置再显示
            WS_CHILD | WS_CLIPSIBLINGS,
            0,
            0,
            1,
            1,
            parent,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null_mut(),
        )
    }
}

/// 把子窗口摆到指定位置（物理像素，相对主窗口客户区）。
pub fn place(host: HWND, x: i32, y: i32, w: i32, h: i32) {
    if host.is_null() {
        return;
    }
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::SetWindowPos(
            host,
            std::ptr::null_mut(),
            x,
            y,
            w.max(1),
            h.max(1),
            windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOZORDER
                | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
        );
    }
}

/// 显示/隐藏。预览收起时藏起来，免得它继续占着那块像素。
pub fn set_visible(host: HWND, visible: bool) {
    if host.is_null() {
        return;
    }
    // SW_SHOWNA：显示但不激活，别把焦点从主窗口抢走
    unsafe { ShowWindow(host, if visible { SW_SHOWNA } else { SW_HIDE }) };
}

/// 销毁子窗口。必须比 mpv 先销毁 —— mpv 还引用着这个句柄。
pub fn destroy(host: HWND) {
    if host.is_null() {
        return;
    }
    unsafe { DestroyWindow(host) };
}
