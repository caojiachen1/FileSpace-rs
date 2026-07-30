// Win11 Snap Layout：在最大化按钮位置覆盖一个本进程的原生子窗口，
// 其 WM_NCHITTEST 返回 HTMAXBUTTON，系统据此在悬停时弹出原生布局面板。
//
// 关键经验（均为实测结论）：
// 1. 不能子类化 WebView2 子窗口——Chrome_WidgetWin_1 等属于 msedgewebview2.exe
//    进程，跨进程 SetWindowSubclass 静默失败；
// 2. 覆盖窗口不能用 WS_EX_LAYERED（无论 alpha 值）——分层窗口的命中测试基于
//    表面 alpha，配合 NOREDIRECTIONBITMAP 无表面时整窗穿透，悬停永远不命中。
//    正确做法是普通 "STATIC" 类子窗口（不绘制任何内容，WebView 合成不受影响），
//    这是 tauri-plugin-snap-layout / decorum 验证过的配方。
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};
use std::sync::OnceLock;

use tauri::{AppHandle, Emitter, Manager};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, IsZoomed, LoadCursorW, SendMessageW, SetCursor, SetWindowPos, HWND_TOP,
    IDC_ARROW, SC_MAXIMIZE, SC_RESTORE, SWP_NOACTIVATE, WINDOW_EX_STYLE, WM_NCHITTEST,
    WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_SETCURSOR,
    WM_SYSCOMMAND, WS_CHILD, WS_VISIBLE,
};

const HTMAXBUTTON: isize = 9;
const SUBCLASS_ID: usize = 0x5AB;

static APP: OnceLock<AppHandle> = OnceLock::new();
// 最大化按钮矩形（主窗口客户区物理像素，前端上报）
static RECT_X: AtomicI32 = AtomicI32::new(0);
static RECT_Y: AtomicI32 = AtomicI32::new(0);
static RECT_W: AtomicI32 = AtomicI32::new(0);
static RECT_H: AtomicI32 = AtomicI32::new(0);
// 覆盖窗口与主窗口句柄（仅主线程创建/移动）
static OVERLAY: AtomicIsize = AtomicIsize::new(0);
static PARENT: AtomicIsize = AtomicIsize::new(0);
// 悬停态（变化时通知前端绘制按钮 hover 背景）
static HOVER: AtomicBool = AtomicBool::new(false);

fn set_hover(h: bool) {
    if HOVER.swap(h, Ordering::Relaxed) != h {
        if let Some(app) = APP.get() {
            let _ = app.emit("snap-hover", h);
        }
    }
}

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
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE | TME_NONCLIENT,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
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
            let parent = HWND(PARENT.load(Ordering::Relaxed) as *mut _);
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

/// 按前端上报的矩形摆放覆盖窗口（须在主线程）
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

/// 前端上报按钮矩形：更新并移动覆盖窗口
pub fn set_rect(x: i32, y: i32, w: i32, h: i32) {
    RECT_X.store(x, Ordering::Relaxed);
    RECT_Y.store(y, Ordering::Relaxed);
    RECT_W.store(w, Ordering::Relaxed);
    RECT_H.store(h, Ordering::Relaxed);
    let overlay = OVERLAY.load(Ordering::Relaxed);
    if overlay != 0 {
        if let Some(app) = APP.get() {
            let _ = app.run_on_main_thread(move || unsafe {
                place_overlay(HWND(overlay as *mut _));
            });
        }
    }
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
        unsafe {
            // 系统 "STATIC" 类：无需注册类，自身不绘制内容，不干扰 WebView 合成
            let Ok(overlay) = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                None,
                WS_CHILD | WS_VISIBLE,
                RECT_X.load(Ordering::Relaxed),
                RECT_Y.load(Ordering::Relaxed),
                RECT_W.load(Ordering::Relaxed),
                RECT_H.load(Ordering::Relaxed),
                Some(parent),
                None,
                None,
                None,
            ) else {
                return;
            };
            let _ = SetWindowSubclass(overlay, Some(snap_subclass_proc), SUBCLASS_ID, 0);
            PARENT.store(parent.0 as isize, Ordering::Relaxed);
            OVERLAY.store(overlay.0 as isize, Ordering::Relaxed);
            place_overlay(overlay);
        }
    });
}
