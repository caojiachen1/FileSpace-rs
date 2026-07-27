// 目录变更监听：SHChangeNotifyRegister -> 隐藏窗口消息 -> Tauri 事件推送前端
use std::cell::Cell;
use std::sync::OnceLock;

use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{HANDLE, LPARAM, WPARAM};
use windows::Win32::UI::Shell::{
    ILFree, SHChangeNotification_Lock, SHChangeNotification_Unlock, SHChangeNotifyDeregister,
    SHChangeNotifyEntry, SHChangeNotifyRegister, SHCNRF_InterruptLevel, SHCNRF_NewDelivery,
    SHCNRF_ShellLevel,
};
use windows::Win32::UI::WindowsAndMessaging::WM_USER;

use crate::shell_items::parse_to_pidl;
use crate::shell_thread::helper_hwnd;

pub const WM_SHNOTIFY: u32 = WM_USER + 0x77;
const SHCNE_ALLEVENTS: i32 = 0x7FFF_FFFF;

static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

thread_local! {
    static WATCH_ID: Cell<u32> = const { Cell::new(0) };
}

pub fn set_app_handle(app: AppHandle) {
    APP_HANDLE.set(app).ok();
}

/// 监听指定文件夹（替换旧的监听）。必须在 STA 线程调用。
pub fn watch(path: &str) {
    unwatch();
    let pidl = match parse_to_pidl(path) {
        Ok(p) => p,
        Err(_) => return,
    };
    unsafe {
        let entry = SHChangeNotifyEntry {
            pidl,
            fRecursive: false.into(),
        };
        let id = SHChangeNotifyRegister(
            helper_hwnd(),
            SHCNRF_ShellLevel | SHCNRF_InterruptLevel | SHCNRF_NewDelivery,
            SHCNE_ALLEVENTS,
            WM_SHNOTIFY,
            1,
            &entry,
        );
        WATCH_ID.with(|w| w.set(id));
        ILFree(Some(pidl));
    }
}

pub fn unwatch() {
    WATCH_ID.with(|w| {
        let id = w.get();
        if id != 0 {
            unsafe {
                let _ = SHChangeNotifyDeregister(id);
            }
            w.set(0);
        }
    });
}

/// 隐藏窗口收到 WM_SHNOTIFY 时调用（NewDelivery 模式必须 Lock/Unlock）
pub fn handle_notify(wparam: WPARAM, lparam: LPARAM) {
    unsafe {
        let hlock = SHChangeNotification_Lock(HANDLE(wparam.0 as _), lparam.0 as u32, None, None);
        if !hlock.is_invalid() {
            let _ = SHChangeNotification_Unlock(hlock);
        }
    }
    if let Some(app) = APP_HANDLE.get() {
        let _ = app.emit("fs-changed", ());
    }
}
