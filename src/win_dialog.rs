//! 对话框窗口的原生行为：挂到主窗口下、在屏幕正中居中、打开期间禁用主窗口。
//!
//! 背景：Slint/winit 建顶层窗口时不指定 owner，也不给坐标，所以对话框默认出现在
//! 左上角、和主窗口没有任何从属关系。Windows 原生模态对话框（`MessageBox` 这类）
//! 靠两件事实现：窗口有 owner（`GWLP_HWNDPARENT`），owner 被 `EnableWindow` 禁用。
//! 做完这两件，系统就会自己给出模态该有的行为 —— 对话框始终浮在 owner 之上、
//! 随 owner 一起最小化，点被禁用的 owner 时闪对话框标题栏。不需要自己画闪烁，
//! 也不需要用低级鼠标钩子去猜用户点了哪里。
//!
//! 麻烦的地方是这些设置都要拿到窗口句柄才能做，而句柄要等事件循环真的把窗口建出来
//! 才有 —— 见 [`open`] 里关于时机的说明。

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{CloseRequestResponse, ComponentHandle, PhysicalPosition, Timer};
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTOPRIMARY,
};
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, GWLP_HWNDPARENT,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, WS_BORDER, WS_CAPTION, WS_CLIPSIBLINGS,
    WS_EX_ACCEPTFILES, WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SIZEBOX, WS_SYSMENU,
};

/// 兜底定时器的延迟：正常路径走窗口事件，等不到事件才会用到它。
const FALLBACK_DELAY: Duration = Duration::from_millis(200);

/// 当前打开的模态对话框数量。主窗口只有一个，用计数是为了支持叠加 —— 比如编码
/// 出错弹提示时另一个对话框还开着。最后一个关掉才恢复主窗口。
static OPEN_MODALS: AtomicUsize = AtomicUsize::new(0);

/// 已打开的模态对话框。持有者负责在每个关闭路径上调 `dismiss()`；漏了也会在组件
/// 销毁、闭包跟着被丢掉时由 `Drop` 兜底。
pub struct ModalState {
    owner: HWND,
    released: Cell<bool>,
}

impl ModalState {
    /// 恢复主窗口的可交互状态。幂等：按钮回调和 `on_close_requested` 只有先到的那个生效。
    pub fn dismiss(&self) {
        if self.released.replace(true) {
            return;
        }
        release_owner(self.owner);
    }
}

impl Drop for ModalState {
    fn drop(&mut self) {
        self.dismiss();
    }
}

/// 显示对话框，并把它变成 `owner` 的模态子窗口、摆到屏幕正中。
///
/// `owner` 传空表示还没有主窗口（启动早期弹「未找到 FFmpeg」），这时只在主显示器
/// 工作区居中，不禁用任何窗口。
///
/// `logical_size` 是对话框的逻辑尺寸，由调用方从 Slint 侧的 `dialog-w` / `dialog-h`
/// 读出来传进来 —— 必须是这两个，不能用 `Window::size()`：那是窗口建好之后才有值的。
///
/// 窗口由本函数负责显示。返回的 `ModalState` 要移进按钮回调，关闭时调一次 `dismiss()`。
/// 用户能不能自己把对话框关掉。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ClosePolicy {
    /// 标题栏 X、Alt+F4 都当关闭（三个普通对话框用）
    Dismissible,
    /// 挡下 X 和 Alt+F4，只能由程序调 `dismiss()` 关（进度框用）
    Blocking,
}

pub fn open<C: ComponentHandle + 'static>(
    comp: &C,
    owner: HWND,
    logical_size: (f32, f32),
) -> Rc<ModalState> {
    open_with(comp, owner, logical_size, ClosePolicy::Dismissible)
}

/// 和 [`open`] 一样，但**用户关不掉**：标题栏的 X 和 Alt+F4 都被挡下，只能由程序
/// 在活干完之后调 [`ModalState::dismiss`]。
///
/// 「正在探测」那个进度框用它 —— 探测亚秒级就结束，给用户一个中途关掉的机会
/// 反而要引入"编码流程被打断"的状态机，不值当。
pub fn open_blocking<C: ComponentHandle + 'static>(
    comp: &C,
    owner: HWND,
    logical_size: (f32, f32),
) -> Rc<ModalState> {
    open_with(comp, owner, logical_size, ClosePolicy::Blocking)
}

fn open_with<C: ComponentHandle + 'static>(
    comp: &C,
    owner: HWND,
    logical_size: (f32, f32),
    close_policy: ClosePolicy,
) -> Rc<ModalState> {
    // 位置要在窗口创建**之前**写进去，否则窗口会先在默认位置（左上角）露一下脸
    place_before_show(comp, owner, logical_size);

    // 禁用主窗口不依赖对话框的句柄，立刻做，别让用户有功夫点到它
    if !owner.is_null() {
        acquire_owner(owner);
    }

    // 剩下的设置（owner 关系、图标、置前）都得等窗口建出来。挂在窗口事件上，等它的
    // 第一个事件再动手：winit 是异步建窗口的，show() 返回时窗口还不存在，而窗口从
    // 创建到第一次 RedrawRequested 之间会先来几个事件（Resized、Focused…）。
    let placed = Rc::new(Cell::new(false));
    {
        let placed = placed.clone();
        let weak = comp.as_weak();
        comp.window().on_winit_window_event(move |_, _| {
            // 只在第一个事件上做一次，之后都是普通事件
            if !placed.replace(true) {
                if let Some(comp) = weak.upgrade() {
                    setup(&comp, owner);
                }
            }
            EventResult::Propagate
        });
    }

    let _ = comp.show();

    // 兜底：万一一个窗口事件都没收到（正常不会），过一会儿补上。已经做过就跳过。
    {
        let placed = placed.clone();
        let weak = comp.as_weak();
        Timer::single_shot(FALLBACK_DELAY, move || {
            if !placed.replace(true) {
                if let Some(comp) = weak.upgrade() {
                    setup(&comp, owner);
                }
            }
        });
    }

    let state = Rc::new(ModalState {
        owner,
        released: Cell::new(false),
    });

    // 点标题栏的 X、Alt+F4 都从这里过。少了这一步，主窗口会一直禁用着。
    let closed = state.clone();
    comp.window().on_close_requested(move || match close_policy {
        ClosePolicy::Dismissible => {
            closed.dismiss();
            CloseRequestResponse::HideWindow
        }
        // 进度框不给关：窗口留着，主窗口也继续禁用着
        ClosePolicy::Blocking => CloseRequestResponse::KeepWindowShown,
    });

    state
}

/// 趁窗口还没建出来，把目标位置写进待创建的窗口属性。
///
/// `set_position` 在窗口存在时是挪窗口，不存在时就是把坐标存下来、等 `CreateWindowEx`
/// 用 —— 这正是我们要的：窗口一出现就在正中，不存在「先闪一下再跳过去」。
fn place_before_show<C: ComponentHandle>(comp: &C, owner: HWND, logical_size: (f32, f32)) {
    let (logical_w, logical_h) = logical_size;
    if logical_w <= 0.0 || logical_h <= 0.0 {
        return; // 读不到尺寸就不猜了，建好之后那次校正会兜住
    }
    let Some(area) = work_area(owner) else { return };

    let dpi = unsafe {
        let d = if owner.is_null() {
            GetDpiForSystem()
        } else {
            GetDpiForWindow(owner)
        };
        if d == 0 {
            96
        } else {
            d
        }
    };
    let scale = dpi as f32 / 96.0;
    let (frame_w, frame_h) = frame_size(dpi);
    let outer_w = (logical_w * scale).round() as i32 + frame_w;
    let outer_h = (logical_h * scale).round() as i32 + frame_h;

    let x = area.left + (area.right - area.left - outer_w) / 2;
    let y = area.top + (area.bottom - area.top - outer_h) / 2;
    comp.window().set_position(PhysicalPosition::new(x, y));
}

/// 外框比客户区大出来的那部分（四周边框 + 标题栏）。
///
/// 传入空客户区矩形，`AdjustWindowRectExForDpi` 会把边框宽度算在负数坐标上，
/// 所以「大出来的宽度」是 right - left。样式照抄 winit 建标准顶层窗口用的那一套
/// （`window_state.rs` 的 `to_window_styles`）：Slint 只改了 visible 和 transparent，
/// 边框相关的旗标一个没动。
fn frame_size(dpi: u32) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let style = WS_CAPTION
        | WS_BORDER
        | WS_CLIPSIBLINGS
        | WS_SYSMENU
        | WS_SIZEBOX
        | WS_MAXIMIZEBOX
        | WS_MINIMIZEBOX;
    let ex_style = WS_EX_WINDOWEDGE | WS_EX_ACCEPTFILES;

    if unsafe { AdjustWindowRectExForDpi(&mut rect, style, 0, ex_style, dpi) } == 0 {
        return (0, 0);
    }
    (rect.right - rect.left, rect.bottom - rect.top)
}

/// 窗口建好之后的一次性设置：套图标、设 owner、居中、置前。
fn setup<C: ComponentHandle>(comp: &C, owner: HWND) {
    let hwnd = crate::win_icon::main_window_hwnd(comp.window());
    if hwnd.is_null() {
        return;
    }

    crate::win_icon::apply_window_icon(hwnd);

    if !owner.is_null() {
        // 顶层窗口的 owner 只能事后补设。MSDN 说 owner 更该在 CreateWindow 时给，
        // 但那要求我们自己建窗口；winit 不给这个机会，运行时设置同样生效。
        unsafe { SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, owner as isize) };
    }

    center_on_screen(hwnd, owner);
    unsafe { SetForegroundWindow(hwnd) };
}

/// 禁用 owner。计数 0→1 时才真的调 `EnableWindow`，叠加打开时只加计数。
fn acquire_owner(owner: HWND) {
    if OPEN_MODALS.fetch_add(1, Ordering::SeqCst) == 0 {
        unsafe { EnableWindow(owner, 0) };
    }
}

/// 恢复 owner。计数 1→0 时才真的恢复。
fn release_owner(owner: HWND) {
    if owner.is_null() {
        return;
    }
    if OPEN_MODALS.fetch_sub(1, Ordering::SeqCst) == 1 {
        unsafe { EnableWindow(owner, 1) };
    }
}

/// 把 dlg 移到「主窗口所在显示器」的工作区正中。没有主窗口时用主显示器。
///
/// 取工作区（`rcWork`）而不是整个显示器（`rcMonitor`），否则对话框会压到任务栏下面。
/// 用外框尺寸（`GetWindowRect`）而不是客户区，视觉上才是正的。
fn center_on_screen(dlg: HWND, owner: HWND) {
    let mut dlg_rect = RECT::default();
    if unsafe { GetWindowRect(dlg, &mut dlg_rect) } == 0 {
        return;
    }
    let Some(area) = work_area(owner) else { return };

    let x = area.left + (area.right - area.left - (dlg_rect.right - dlg_rect.left)) / 2;
    let y = area.top + (area.bottom - area.top - (dlg_rect.bottom - dlg_rect.top)) / 2;

    // 传 owner 当 hwndInsertAfter，把对话框排到它正上方：owner 关系是事后补设的，
    // 靠这一步再确认一次层叠顺序。SWP_NOACTIVATE 是因为窗口已经在前台，只挪位置。
    let mut flags = SWP_NOSIZE | SWP_NOACTIVATE;
    if owner.is_null() {
        flags |= SWP_NOZORDER;
    }
    unsafe { SetWindowPos(dlg, owner, x, y, 0, 0, flags) };
}

/// owner 所在显示器的工作区；owner 为空时取主显示器。拿不到就返回 None。
fn work_area(owner: HWND) -> Option<RECT> {
    let monitor = unsafe {
        if owner.is_null() {
            MonitorFromPoint(
                windows_sys::Win32::Foundation::POINT { x: 0, y: 0 },
                MONITOR_DEFAULTTOPRIMARY,
            )
        } else {
            MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST)
        }
    };
    if monitor.is_null() {
        return None;
    }
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }
    Some(info.rcWork)
}
