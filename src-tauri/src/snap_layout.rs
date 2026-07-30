// Win11 Snap Layout + 最大化标题栏顶边修复：
//
// 1) 最大化按钮覆盖窗口：WM_NCHITTEST 返回 HTMAXBUTTON，悬停弹出系统 Snap 面板。
// 2) 顶边条状覆盖窗口（仅最大化时显示）：屏蔽 WebView2 的非客户区 resize 带。
//    该带由 wry 开启的 SetIsNonClientRegionSupportEnabled(true) 在 WebView2 进程内
//    实现，实时检查宿主 WS_SIZEBOX，宿主侧命中测试拦截不到；运行时关闭该设置又
//    要"下一次导航"才生效（SPA 永不导航）。唯一可靠方案是用本进程窗口盖住顶边：
//    关闭区→HTCLOSE（屏幕右上角可直接点中，Fitts）、最大化区→HTMAXBUTTON（Snap
//    在顶边也可触发）、最小化区→HTMINBUTTON、其余→HTCAPTION（箭头光标+原生拖拽）。
//
// 关键经验（均为实测结论）：
// - 不能子类化 WebView2 子窗口（属 msedgewebview2.exe 进程，跨进程静默失败）；
// - 覆盖窗口不能用 WS_EX_LAYERED（命中测试整窗穿透），须用普通 "STATIC" 类子窗口；
// - 不能用 setResizable(false)（摘 WS_SIZEBOX 后系统判定不可贴靠，Snap 全灭）。
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU8, Ordering};
use std::sync::OnceLock;

use tauri::{AppHandle, Emitter, Manager};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, GetClientRect, GetSystemMetrics, IsZoomed, LoadCursorW, PostMessageW,
    SendMessageW, SetCursor, SetWindowPos, ShowWindow, HWND_TOP, IDC_ARROW, SC_CLOSE,
    SC_MAXIMIZE, SC_MINIMIZE, SC_RESTORE, SM_CXPADDEDBORDER, SM_CYSIZEFRAME, SWP_NOACTIVATE,
    SW_HIDE, SW_SHOWNA, WINDOW_EX_STYLE, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN,
    WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_SETCURSOR, WM_SYSCOMMAND, WS_CHILD, WS_VISIBLE,
};

const HTCAPTION: isize = 2;
const HTMINBUTTON: isize = 8;
const HTMAXBUTTON: isize = 9;
const HTCLOSE: isize = 20;
const SUBCLASS_ID: usize = 0x5AB;
const STRIP_SUBCLASS_ID: usize = 0x5AD;

static APP: OnceLock<AppHandle> = OnceLock::new();
// 最大化按钮矩形（主窗口客户区物理像素，前端上报）
static RECT_X: AtomicI32 = AtomicI32::new(0);
static RECT_Y: AtomicI32 = AtomicI32::new(0);
static RECT_W: AtomicI32 = AtomicI32::new(0);
static RECT_H: AtomicI32 = AtomicI32::new(0);
// 最小化/关闭按钮的水平范围（顶边条分段命中用）
static MIN_X: AtomicI32 = AtomicI32::new(0);
static MIN_W: AtomicI32 = AtomicI32::new(0);
static CLOSE_X: AtomicI32 = AtomicI32::new(0);
static CLOSE_W: AtomicI32 = AtomicI32::new(0);
// 覆盖窗口/顶边条/主窗口句柄（仅主线程创建与移动）
static OVERLAY: AtomicIsize = AtomicIsize::new(0);
static STRIP: AtomicIsize = AtomicIsize::new(0);
static PARENT: AtomicIsize = AtomicIsize::new(0);
// Snap 悬停态（变化时通知前端绘制最大化按钮 hover 背景）
static HOVER: AtomicBool = AtomicBool::new(false);
// 顶边条当前悬停按钮：0 无 / 1 最小化 / 3 关闭（变化时通知前端）
static STRIP_BTN: AtomicU8 = AtomicU8::new(0);
// 最大化态（前端 tao isMaximized 上报，决定顶边条显隐）
static MAXIMIZED: AtomicBool = AtomicBool::new(false);

fn set_hover(h: bool) {
    if HOVER.swap(h, Ordering::Relaxed) != h {
        if let Some(app) = APP.get() {
            let _ = app.emit("snap-hover", h);
        }
    }
}

fn set_strip_btn(b: u8) {
    if STRIP_BTN.swap(b, Ordering::Relaxed) != b {
        if let Some(app) = APP.get() {
            let _ = app.emit("nc-btn-hover", match b {
                1 => "min",
                3 => "close",
                _ => "",
            });
        }
    }
}

unsafe fn track_leave(hwnd: HWND) {
    let mut tme = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE | TME_NONCLIENT,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    let _ = TrackMouseEvent(&mut tme);
}

fn parent_hwnd() -> HWND {
    HWND(PARENT.load(Ordering::Relaxed) as *mut _)
}

/* ===================== 最大化按钮覆盖窗口 ===================== */

unsafe extern "system" fn snap_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        // 整个覆盖窗口都是"最大化按钮"；首次进入时注册离开通知
        WM_NCHITTEST => {
            if !HOVER.load(Ordering::Relaxed) {
                set_hover(true);
                track_leave(hwnd);
            }
            LRESULT(HTMAXBUTTON)
        }
        WM_NCMOUSELEAVE => {
            set_hover(false);
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        // 吞掉按下/双击；抬起时走系统命令最大化/还原（tao 能正确感知状态）
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wparam.0 as isize == HTMAXBUTTON => LRESULT(0),
        WM_NCLBUTTONUP if wparam.0 as isize == HTMAXBUTTON => {
            let parent = parent_hwnd();
            let cmd = if IsZoomed(parent).as_bool() { SC_RESTORE } else { SC_MAXIMIZE };
            let _ = SendMessageW(parent, WM_SYSCOMMAND, Some(WPARAM(cmd as usize)), None);
            set_hover(false);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if let Ok(cur) = LoadCursorW(None, IDC_ARROW) {
                let _ = SetCursor(Some(cur));
            }
            LRESULT(1)
        }
        _ => DefSubclassProc(hwnd, msg, wparam, lparam),
    }
}

/* ===================== 顶边条覆盖窗口（仅最大化时显示） ===================== */

/// 顶边条内按 x 坐标分段：关闭/最大化/最小化按钮区或标题栏
fn strip_zone(x: i32) -> isize {
    let within = |bx: i32, bw: i32| bw > 0 && x >= bx && x < bx + bw;
    if within(CLOSE_X.load(Ordering::Relaxed), CLOSE_W.load(Ordering::Relaxed)) {
        HTCLOSE
    } else if within(RECT_X.load(Ordering::Relaxed), RECT_W.load(Ordering::Relaxed)) {
        HTMAXBUTTON
    } else if within(MIN_X.load(Ordering::Relaxed), MIN_W.load(Ordering::Relaxed)) {
        HTMINBUTTON
    } else {
        HTCAPTION
    }
}

unsafe extern "system" fn strip_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_NCHITTEST => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut pt = POINT { x, y };
            let _ = ScreenToClient(hwnd, &mut pt);
            let zone = strip_zone(pt.x);
            set_hover(zone == HTMAXBUTTON);
            set_strip_btn(match zone {
                HTMINBUTTON => 1,
                HTCLOSE => 3,
                _ => 0,
            });
            track_leave(hwnd);
            LRESULT(zone)
        }
        WM_NCMOUSELEAVE => {
            set_hover(false);
            set_strip_btn(0);
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        // 标题栏区：转发给主窗口做原生拖拽/双击（还原等系统行为）
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wparam.0 as isize == HTCAPTION => {
            let _ = PostMessageW(Some(parent_hwnd()), msg, wparam, lparam);
            LRESULT(0)
        }
        // 按钮区：吞掉按下/双击，抬起时执行对应系统命令
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK => LRESULT(0),
        WM_NCLBUTTONUP => {
            let parent = parent_hwnd();
            let cmd = match wparam.0 as isize {
                HTCLOSE => SC_CLOSE,
                HTMINBUTTON => SC_MINIMIZE,
                HTMAXBUTTON => {
                    if IsZoomed(parent).as_bool() { SC_RESTORE } else { SC_MAXIMIZE }
                }
                _ => return DefSubclassProc(hwnd, msg, wparam, lparam),
            };
            set_hover(false);
            set_strip_btn(0);
            let _ = SendMessageW(parent, WM_SYSCOMMAND, Some(WPARAM(cmd as usize)), None);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if let Ok(cur) = LoadCursorW(None, IDC_ARROW) {
                let _ = SetCursor(Some(cur));
            }
            LRESULT(1)
        }
        _ => DefSubclassProc(hwnd, msg, wparam, lparam),
    }
}

/* ===================== 布局与状态同步 ===================== */

/// 摆放最大化按钮覆盖窗口（须在主线程）
unsafe fn place_overlay(overlay: HWND) {
    let _ = SetWindowPos(
        overlay,
        Some(HWND_TOP),
        RECT_X.load(Ordering::Relaxed),
        RECT_Y.load(Ordering::Relaxed),
        RECT_W.load(Ordering::Relaxed),
        RECT_H.load(Ordering::Relaxed),
        SWP_NOACTIVATE,
    );
}

/// 摆放顶边条：全宽、高度为系统 resize 带厚度（须在主线程）
unsafe fn place_strip(strip: HWND) {
    let parent = parent_hwnd();
    let mut rc = RECT::default();
    let _ = GetClientRect(parent, &mut rc);
    let h = GetSystemMetrics(SM_CYSIZEFRAME) + GetSystemMetrics(SM_CXPADDEDBORDER);
    let _ = SetWindowPos(strip, Some(HWND_TOP), 0, 0, rc.right, h.max(1), SWP_NOACTIVATE);
}

/// 同步顶边条显隐与位置（仅最大化时显示；须在主线程）
unsafe fn sync_strip() {
    let strip = STRIP.load(Ordering::Relaxed);
    if strip == 0 {
        return;
    }
    let strip = HWND(strip as *mut _);
    if MAXIMIZED.load(Ordering::Relaxed) {
        place_strip(strip);
        let _ = ShowWindow(strip, SW_SHOWNA);
    } else {
        let _ = ShowWindow(strip, SW_HIDE);
    }
}

fn on_main(f: impl FnOnce() + Send + 'static) {
    if let Some(app) = APP.get() {
        let _ = app.run_on_main_thread(f);
    }
}

/// 前端上报最大化按钮矩形：更新覆盖窗口与顶边条
pub fn set_rect(x: i32, y: i32, w: i32, h: i32) {
    RECT_X.store(x, Ordering::Relaxed);
    RECT_Y.store(y, Ordering::Relaxed);
    RECT_W.store(w, Ordering::Relaxed);
    RECT_H.store(h, Ordering::Relaxed);
    let overlay = OVERLAY.load(Ordering::Relaxed);
    if overlay != 0 {
        on_main(move || unsafe {
            place_overlay(HWND(overlay as *mut _));
            sync_strip();
        });
    }
}

/// 前端上报最小化/关闭按钮的水平范围（顶边条分段命中用）
pub fn set_caption_rects(min_x: i32, min_w: i32, close_x: i32, close_w: i32) {
    MIN_X.store(min_x, Ordering::Relaxed);
    MIN_W.store(min_w, Ordering::Relaxed);
    CLOSE_X.store(close_x, Ordering::Relaxed);
    CLOSE_W.store(close_w, Ordering::Relaxed);
}

/// 前端上报最大化状态（tao isMaximized）：决定顶边条显隐
pub fn set_maximized(m: bool) {
    MAXIMIZED.store(m, Ordering::Relaxed);
    on_main(|| unsafe { sync_strip() });
}

/// 创建覆盖窗口（须在主线程；WebView2 已创建后调用，保证盖在其上方）
pub fn init(app: &AppHandle) {
    APP.set(app.clone()).ok();
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if OVERLAY.load(Ordering::Relaxed) != 0 {
            return;
        }
        let Some(parent) = handle
            .get_webview_window("main")
            .and_then(|w| w.hwnd().ok())
            .map(|h| HWND(h.0))
        else {
            return;
        };
        PARENT.store(parent.0 as isize, Ordering::Relaxed);
        unsafe {
            // 系统 "STATIC" 类：无需注册类，自身不绘制内容，不干扰 WebView 合成
            let make = |visible: bool| {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    None,
                    if visible { WS_CHILD | WS_VISIBLE } else { WS_CHILD },
                    0,
                    0,
                    0,
                    0,
                    Some(parent),
                    None,
                    None,
                    None,
                )
            };
            // 最大化按钮覆盖窗口（常驻）
            if let Ok(overlay) = make(true) {
                let _ = SetWindowSubclass(overlay, Some(snap_subclass_proc), SUBCLASS_ID, 0);
                OVERLAY.store(overlay.0 as isize, Ordering::Relaxed);
                place_overlay(overlay);
            }
            // 顶边条（仅最大化时显示）
            if let Ok(strip) = make(false) {
                let _ = SetWindowSubclass(strip, Some(strip_subclass_proc), STRIP_SUBCLASS_ID, 0);
                STRIP.store(strip.0 as isize, Ordering::Relaxed);
            }
            sync_strip();
        }
    });
}
