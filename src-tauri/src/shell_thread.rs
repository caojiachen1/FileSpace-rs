// 专用 STA 线程：所有 Shell COM 调用都在此线程执行。
// 同时创建一个隐藏窗口，供 IContextMenu2/3 消息转发（"打开方式"、"发送到"等子菜单需要）。
use std::cell::RefCell;
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
use std::time::Duration;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Ole::OleInitialize;
use windows::Win32::UI::Shell::{IContextMenu2, IContextMenu3};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW, RegisterClassW,
    TranslateMessage, CS_HREDRAW, CS_VREDRAW, MSG, PM_REMOVE,
    WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR, WM_MENUSELECT, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

type Job = Box<dyn FnOnce() + Send>;

static JOB_SENDER: OnceLock<Sender<Job>> = OnceLock::new();

thread_local! {
    // 当前正在弹出的上下文菜单（用于消息转发）
    pub static ACTIVE_MENU2: RefCell<Option<IContextMenu2>> = const { RefCell::new(None) };
    pub static ACTIVE_MENU3: RefCell<Option<IContextMenu3>> = const { RefCell::new(None) };
    pub static HELPER_HWND: RefCell<HWND> = const { RefCell::new(HWND(std::ptr::null_mut())) };
}

unsafe extern "system" fn helper_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        crate::shell_watch::WM_SHNOTIFY => {
            crate::shell_watch::handle_notify(wparam, lparam);
            LRESULT(0)
        }
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR | WM_MENUSELECT => {
            // 优先 IContextMenu3::HandleMenuMsg2
            let handled3 = ACTIVE_MENU3.with(|m| {
                if let Some(menu) = m.borrow().as_ref() {
                    let mut result = LRESULT(0);
                    if menu.HandleMenuMsg2(msg, wparam, lparam, Some(&mut result)).is_ok() {
                        return Some(result);
                    }
                }
                None
            });
            if let Some(r) = handled3 {
                return r;
            }
            let handled2 = ACTIVE_MENU2.with(|m| {
                if let Some(menu) = m.borrow().as_ref() {
                    if menu.HandleMenuMsg(msg, wparam, lparam).is_ok() {
                        return true;
                    }
                }
                false
            });
            if handled2 {
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn create_helper_window() -> HWND {
    unsafe {
        let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap();
        let class_name = w!("FileSpaceShellHelper");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(helper_wndproc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wc);
        // 注意：不能用 message-only 窗口（HWND_MESSAGE），否则无法成为前台窗口，
        // TrackPopupMenu 将收不到键盘/失焦事件，Esc 和点击外部都无法关闭菜单
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name,
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
        .unwrap_or(HWND(std::ptr::null_mut()))
    }
}

/// 启动 STA Shell 线程
pub fn start() {
    let (tx, rx) = mpsc::channel::<Job>();
    JOB_SENDER.set(tx).ok();

    std::thread::Builder::new()
        .name("shell-sta".into())
        .spawn(move || {
            unsafe {
                // 必须用 OleInitialize（内含 STA CoInitializeEx）：剪切/复制/粘贴 verb
                // 内部走 OleSetClipboard/OleGetClipboard，仅 CoInitializeEx 会以
                // CO_E_NOTINITIALIZED 静默失败，表现为复制后无法粘贴
                let _ = OleInitialize(None);
            }
            let hwnd = create_helper_window();
            HELPER_HWND.with(|h| *h.borrow_mut() = hwnd);

            loop {
                // 泵消息（菜单转发所需）
                unsafe {
                    let mut msg = MSG::default();
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                match rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(job) => job(),
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .expect("failed to spawn shell thread");
}

/// 在 STA 线程上执行闭包并同步返回结果
pub fn run_on_shell<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    let (tx, rx) = mpsc::channel();
    JOB_SENDER
        .get()
        .expect("shell thread not started")
        .send(Box::new(move || {
            let _ = tx.send(f());
        }))
        .expect("shell thread gone");
    rx.recv().expect("shell job dropped")
}

/// 获取隐藏辅助窗口（仅在 STA 线程内调用）
pub fn helper_hwnd() -> HWND {
    HELPER_HWND.with(|h| *h.borrow())
}
