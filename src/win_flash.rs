//! 关于窗口打开时，检测「点击被禁用的主窗口」并在关于窗口的标题栏/边框播放闪烁。
//!
//! 背景：主窗口用 winit `set_enable(false)`（即 `EnableWindow`）禁用后，Win32 不会
//! 把鼠标消息投递给被禁用的窗口 —— 应用层收不到任何点击，无法知道用户点了主窗口。
//! 而 OS 自带的「点击被禁用窗口 → 闪烁其 owned 对话框标题栏」反馈只作用于原生
//! caption（no-frame 窗口没有 caption），因此在无边框的自绘 Aero 标题栏上完全看不到。
//!
//! 方案：装一个 `WH_MOUSE_LL` 低级鼠标钩子。它在输入层看到所有鼠标消息（包括落在
//! 被禁用窗口上的），命中「关于窗口打开 + 按下点经 `WindowFromPoint` 命中主窗口」
//! 时，经 `Weak::upgrade_in_event_loop` 在事件循环线程上把关于窗口的 `flash` 置 true，
//! 80ms 后经 `Timer::single_shot` 复位 —— Slint 端用状态迁移实现「瞬时全亮、缓出
//! 淡出」。钩子只安装一次，About 关闭后由 `ABOUT_OPEN` 关闸进入休眠空转。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use slint::{ComponentHandle, Timer};
use windows_sys::Win32::Foundation::{HWND, LRESULT, LPARAM, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, HHOOK, MSLLHOOKSTRUCT, SetWindowsHookExW, WindowFromPoint, WH_MOUSE_LL,
    WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_RBUTTONDOWN, WM_XBUTTONDOWN,
};

/// 关于窗口是否打开（钩子只在打开期间触发闪烁，关闭后为空转）。
static ABOUT_OPEN: AtomicBool = AtomicBool::new(false);

/// 一次闪烁是否正在进行（防止快速连点把闪烁掐断/重复叠加）。
static FLASHING: AtomicBool = AtomicBool::new(false);

/// 主窗口 HWND（命中测试的目标；物理像素空间）。
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

/// 钩子句柄（只安装一次，App 生命周期内不卸载；存裸指针，0 表示未安装）。
static HOOK_RAW: AtomicIsize = AtomicIsize::new(0);

/// 触发闪烁用的关于窗口弱引用（钩子回调无法捕获上下文，只能经全局取）。
static ABOUT_WEAK: Mutex<Option<slint::Weak<crate::AboutDialog>>> = Mutex::new(None);

/// 关于窗口打开时调用：记录主窗口句柄与对话框弱引用，并幂等地安装钩子。
/// 必须在事件循环线程调用（取 HWND / 创建 Weak 都需要）。
pub fn about_opened(main_hwnd: HWND, about: &crate::AboutDialog) {
    MAIN_HWND.store(main_hwnd as isize, Ordering::SeqCst);
    *ABOUT_WEAK.lock().unwrap() = Some(about.as_weak());
    ABOUT_OPEN.store(true, Ordering::SeqCst);

    if HOOK_RAW.load(Ordering::SeqCst) != 0 {
        return; // 已安装
    }
    // WH_MOUSE_LL 是全局低级钩子：hmod 传 NULL、线程 0。回调运行在安装线程
    // （= 事件循环线程）的消息泵里，因此可以直接读写上面的原子状态。
    unsafe {
        let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(flash_hook_proc), std::ptr::null_mut(), 0);
        HOOK_RAW.store(hook as isize, Ordering::SeqCst);
    }
}

/// 关于窗口关闭时调用：钩子进入休眠（不再触发闪烁）。
pub fn about_closed() {
    ABOUT_OPEN.store(false, Ordering::SeqCst);
}

unsafe extern "system" fn flash_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // nCode < 0：钩子必须原样交给下一个钩子
    if code >= 0 {
        let msg = wparam as u32;
        if matches!(
            msg,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
        ) && ABOUT_OPEN.load(Ordering::SeqCst)
            && !FLASHING.swap(true, Ordering::SeqCst)
        {
            let is_main = unsafe {
                let info = &*(lparam as *const MSLLHOOKSTRUCT);
                WindowFromPoint(info.pt) as isize == MAIN_HWND.load(Ordering::SeqCst)
            };
            if is_main {
                trigger_flash();
            } else {
                // 点击落在其它窗口（如关于窗口自身），释放闪烁锁
                FLASHING.store(false, Ordering::SeqCst);
            }
        }
    }
    unsafe { CallNextHookEx(HOOK_RAW.load(Ordering::SeqCst) as HHOOK, code, wparam, lparam) }
}

/// 在事件循环线程上把 about.flash 置 true，80ms 后复位并释放闪烁锁。
fn trigger_flash() {
    let weak = ABOUT_WEAK.lock().unwrap().clone();
    match weak {
        Some(w) => {
            let w_off = w.clone();
            let result = w.upgrade_in_event_loop(move |dlg| {
                dlg.set_flash(true);
                Timer::single_shot(Duration::from_millis(80), move || {
                    if let Some(d) = w_off.upgrade() {
                        d.set_flash(false);
                    }
                    FLASHING.store(false, Ordering::SeqCst);
                });
            });
            if result.is_err() {
                // 事件循环不可用：闭包不会执行，手动释放闪烁锁
                FLASHING.store(false, Ordering::SeqCst);
            }
        }
        None => {
            FLASHING.store(false, Ordering::SeqCst);
        }
    }
}
