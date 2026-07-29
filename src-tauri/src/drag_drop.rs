// 原生 OLE 拖拽：与资源管理器完全一致的拖拽行为与外观
// - 拖出：SHDoDragDrop（自带 shell 拖拽图像 + 默认 IDropSource）
// - 拖入：接管 WebView2 子窗口的 IDropTarget（RevokeDragDrop + RegisterDragDrop），
//   IDropTargetHelper 转发保证原生拖拽图像渲染，DROPDESCRIPTION 提供"移动到 X"提示文字
// - Drop：转发给目标文件夹自身的 IDropTarget（BHID_SFUIObject），
//   冲突对话框/进度 UI/撤销/回收站/zip 语义全部由 shell 原生处理
//
// 注意：所有 COM 交互都发生在 Tauri 主线程（wry 已将其初始化为 STA 且持续泵消息）。
// 刻意不放 shell STA 线程——DoDragDrop 模态循环会阻塞所有 run_on_shell 命令。
use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, Once};
use std::time::Instant;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use windows::core::{implement, w, BOOL};
use windows::Win32::Foundation::{
    GlobalFree, DRAGDROP_E_INVALIDHWND, HWND, LPARAM, POINT, POINTL,
};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, IDataObject, CLSCTX_INPROC_SERVER, DVASPECT_CONTENT,
    FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    IDropSource, IDropTarget, IDropTarget_Impl, OleInitialize, RegisterDragDrop, ReleaseStgMedium,
    RevokeDragDrop, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE,
    DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{
    DragQueryFileW, IDropTargetHelper, IShellItem, IShellItemArray,
    SHCreateShellItemArrayFromDataObject, SHDoDragDrop, BHID_DataObject, BHID_SFUIObject,
    CLSID_DragDropHelper, DROPDESCRIPTION, DROPIMAGETYPE, DROPIMAGE_COPY, DROPIMAGE_INVALID,
    DROPIMAGE_LINK, DROPIMAGE_MOVE, HDROP, SIGDN, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::EnumChildWindows;

/* ===================== 共享状态 ===================== */

#[derive(Clone)]
struct DropTargetInfo {
    /// "item" | "background" | "pin"
    kind: String,
    parse_path: String,
    fs_path: Option<String>,
    name: String,
    /// 目标是文件型容器（zip/cab 等）：资源管理器对其恒为复制
    is_container_file: bool,
}

#[derive(Clone)]
struct SourceInfo {
    parse_path: String,
    fs_path: Option<String>,
    /// 真实文件夹（zip 等文件型容器不算）：决定能否固定到快速访问
    is_dir: bool,
}

#[derive(Default)]
struct DragState {
    target: Option<DropTargetInfo>,
    sources: Vec<SourceInfo>,
    /// DragEnter 时源允许的 effect 掩码
    allowed: u32,
    /// DROPDESCRIPTION 去抖
    last_desc: Option<(i32, String, String)>,
}

static STATE: LazyLock<Mutex<DragState>> = LazyLock::new(|| Mutex::new(DragState::default()));
static DRAG_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

thread_local! {
    // 仅主线程：注册的 IDropTarget（防释放）与当前拖拽会话的 IDataObject
    static REGISTERED: RefCell<Vec<IDropTarget>> = const { RefCell::new(Vec::new()) };
    static CURRENT_DOBJ: RefCell<Option<IDataObject>> = const { RefCell::new(None) };
    static LAST_OVER_EMIT: RefCell<Option<Instant>> = const { RefCell::new(None) };
}

#[derive(Serialize, Clone)]
struct DragPos {
    x: i32,
    y: i32,
}

/* ===================== 对外接口 ===================== */

/// 前端就绪后调用：在主线程接管 WebView2 全部子窗口的拖放
pub fn init(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || unsafe { init_on_main(&handle) });
}

/// 前端命中测试结果写入（拖拽期间高频调用，绝不可走 run_on_shell）
pub fn update_drop_target(kind: String, parse_path: String, fs_path: Option<String>, name: String) {
    let mut st = STATE.lock().unwrap();
    if kind == "none" {
        st.target = None;
    } else {
        // zip/cab 等文件型容器（含其内部子路径）：自身或任一祖先是真实文件
        let is_container_file = fs_path.as_deref().map(has_file_component).unwrap_or(false);
        st.target = Some(DropTargetInfo { kind, parse_path, fs_path, name, is_container_file });
    }
}

/// 路径自身或任一祖先是真实文件（zip 内部路径的判定依据）
fn has_file_component(p: &str) -> bool {
    let mut cur = Some(std::path::Path::new(p));
    while let Some(c) = cur {
        if c.is_file() {
            return true;
        }
        if c.exists() {
            return false;
        }
        cur = c.parent();
    }
    false
}

/// 启动拖出（应用内 + 拖到外部程序）
pub fn start_drag(app: AppHandle, paths: Vec<String>) {
    if paths.is_empty() || DRAG_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        return;
    }
    let handle = app.clone();
    let r = app.run_on_main_thread(move || {
        let eff = unsafe { do_drag(&handle, &paths) };
        {
            let mut st = STATE.lock().unwrap();
            st.sources.clear();
            st.target = None;
            st.last_desc = None;
            st.allowed = 0;
        }
        DRAG_IN_PROGRESS.store(false, Ordering::SeqCst);
        let _ = handle.emit("fs-drag-finished", eff);
    });
    if r.is_err() {
        DRAG_IN_PROGRESS.store(false, Ordering::SeqCst);
    }
}

/* ===================== 拖出（drag source） ===================== */

unsafe fn do_drag(app: &AppHandle, paths: &[String]) -> u32 {
    let Ok(arr) = crate::shell_menu::selection_array(paths) else {
        return 0;
    };
    let Ok(dobj) = arr.BindToHandler::<_, IDataObject>(None, &BHID_DataObject) else {
        return 0;
    };
    {
        let mut st = STATE.lock().unwrap();
        st.sources = sources_from_array(&arr);
        st.target = None;
        st.last_desc = None;
    }
    let hwnd = main_hwnd(app);
    match SHDoDragDrop(
        hwnd,
        &dobj,
        None::<&IDropSource>,
        DROPEFFECT_MOVE | DROPEFFECT_COPY | DROPEFFECT_LINK,
    ) {
        Ok(e) => e.0,
        Err(_) => 0,
    }
}

fn main_hwnd(app: &AppHandle) -> Option<HWND> {
    app.get_webview_window("main")
        .and_then(|w| w.hwnd().ok())
        .map(|h| HWND(h.0))
}

/* ===================== 初始化（主线程） ===================== */

unsafe fn init_on_main(app: &AppHandle) {
    static OLE_ONCE: Once = Once::new();
    OLE_ONCE.call_once(|| {
        // dragDropEnabled:false 时 tao/wry 均未初始化 OLE，须自行初始化（S_FALSE 亦可）
        let _ = OleInitialize(None);
    });
    let Some(hwnd) = main_hwnd(app) else { return };
    let helper: Option<IDropTargetHelper> =
        CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER).ok();

    REGISTERED.with(|r| r.borrow_mut().clear());
    register_target(hwnd, app, hwnd, helper.as_ref());

    // 枚举全部后代窗口（含 WebView2 的 Chrome_RenderWidgetHostHWND），
    // 摘掉 Chromium 自己的 IDropTarget 并换成我们的
    let mut cb = |child: HWND| -> bool {
        register_target(child, app, hwnd, helper.as_ref());
        true
    };
    let mut trait_obj: &mut dyn FnMut(HWND) -> bool = &mut cb;
    let ptr = &mut trait_obj as *mut _ as *mut c_void;
    unsafe extern "system" fn enum_cb(h: HWND, l: LPARAM) -> BOOL {
        let closure = &mut *(l.0 as *mut &mut dyn FnMut(HWND) -> bool);
        closure(h).into()
    }
    let _ = EnumChildWindows(Some(hwnd), Some(enum_cb), LPARAM(ptr as isize));
}

fn register_target(child: HWND, app: &AppHandle, main: HWND, helper: Option<&IDropTargetHelper>) {
    let dt: IDropTarget = FsDropTarget {
        app: app.clone(),
        main_hwnd: main,
        helper: helper.cloned(),
    }
    .into();
    unsafe {
        if RevokeDragDrop(child) != Err(DRAGDROP_E_INVALIDHWND.into())
            && RegisterDragDrop(child, &dt).is_ok()
        {
            REGISTERED.with(|r| r.borrow_mut().push(dt));
        }
    }
}

/* ===================== IDropTarget 实现 ===================== */

#[implement(IDropTarget)]
struct FsDropTarget {
    app: AppHandle,
    main_hwnd: HWND,
    helper: Option<IDropTargetHelper>,
}

impl FsDropTarget {
    fn client_pos(&self, pt: &POINTL) -> (i32, i32) {
        let mut p = POINT { x: pt.x, y: pt.y };
        unsafe {
            let _ = ScreenToClient(self.main_hwnd, &mut p);
        }
        (p.x, p.y)
    }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for FsDropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: windows::core::Ref<'_, IDataObject>,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let Some(dobj) = pdataobj.as_ref().cloned() else {
            unsafe { *pdweffect = DROPEFFECT_NONE };
            return Ok(());
        };
        let allowed = unsafe { (*pdweffect).0 };
        let srcs = unsafe { extract_sources(&dobj) };
        {
            let mut st = STATE.lock().unwrap();
            st.allowed = allowed;
            // 应用内拖拽已由 start_drag 预填精确 parse_path，不覆盖
            if !DRAG_IN_PROGRESS.load(Ordering::SeqCst) || st.sources.is_empty() {
                st.sources = srcs;
            }
            st.last_desc = None;
        }
        CURRENT_DOBJ.with(|c| *c.borrow_mut() = Some(dobj.clone()));

        let eff = compute_effect(grfkeystate.0, &STATE.lock().unwrap());
        unsafe {
            if let Some(h) = &self.helper {
                let p = POINT { x: pt.x, y: pt.y };
                let _ = h.DragEnter(self.main_hwnd, &dobj, &p, eff);
            }
            *pdweffect = eff;
        }
        let (x, y) = self.client_pos(pt);
        let _ = self.app.emit("fs-drag-enter", DragPos { x, y });
        Ok(())
    }

    fn DragOver(
        &self,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let eff = compute_effect(grfkeystate.0, &STATE.lock().unwrap());
        CURRENT_DOBJ.with(|c| {
            if let Some(d) = c.borrow().as_ref() {
                let (img, msg, insert) = desc_for(eff);
                unsafe { set_drop_description(d, img, &msg, &insert) };
            }
        });
        unsafe {
            if let Some(h) = &self.helper {
                let p = POINT { x: pt.x, y: pt.y };
                let _ = h.DragOver(&p, eff);
            }
            *pdweffect = eff;
        }
        // 节流 emit（前端做命中测试与高亮）
        let due = LAST_OVER_EMIT.with(|t| {
            let mut t = t.borrow_mut();
            if t.map(|i| i.elapsed().as_millis() >= 16).unwrap_or(true) {
                *t = Some(Instant::now());
                true
            } else {
                false
            }
        });
        if due {
            let (x, y) = self.client_pos(pt);
            let _ = self.app.emit("fs-drag-over", DragPos { x, y });
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        CURRENT_DOBJ.with(|c| {
            if let Some(d) = c.borrow_mut().take() {
                unsafe { set_drop_description(&d, DROPIMAGE_INVALID, "", "") };
            }
        });
        unsafe {
            if let Some(h) = &self.helper {
                let _ = h.DragLeave();
            }
        }
        {
            let mut st = STATE.lock().unwrap();
            st.target = None;
            st.last_desc = None;
            if !DRAG_IN_PROGRESS.load(Ordering::SeqCst) {
                st.sources.clear();
                st.allowed = 0;
            }
        }
        let _ = self.app.emit("fs-drag-leave", ());
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: windows::core::Ref<'_, IDataObject>,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let dobj = pdataobj.as_ref().cloned();
        let eff = compute_effect(grfkeystate.0, &STATE.lock().unwrap());
        unsafe {
            if let (Some(h), Some(d)) = (&self.helper, &dobj) {
                let p = POINT { x: pt.x, y: pt.y };
                let _ = h.Drop(d, &p, eff);
            }
        }
        if eff != DROPEFFECT_NONE {
            let target = STATE.lock().unwrap().target.clone();
            if let (Some(d), Some(t)) = (&dobj, target) {
                unsafe { perform_drop(&self.app, d, &t, grfkeystate, *pt, eff) };
            }
        }
        unsafe { *pdweffect = eff };
        CURRENT_DOBJ.with(|c| c.borrow_mut().take());
        {
            let mut st = STATE.lock().unwrap();
            st.target = None;
            st.last_desc = None;
            if !DRAG_IN_PROGRESS.load(Ordering::SeqCst) {
                st.sources.clear();
                st.allowed = 0;
            }
        }
        let _ = self.app.emit("fs-drag-drop", ());
        Ok(())
    }
}

/* ===================== Drop 执行 ===================== */

unsafe fn perform_drop(
    app: &AppHandle,
    dobj: &IDataObject,
    t: &DropTargetInfo,
    keys: MODIFIERKEYS_FLAGS,
    pt: POINTL,
    eff: DROPEFFECT,
) {
    if t.kind == "pin" {
        // 拖到快速访问 → 固定（走 shell 线程，此时主线程模态循环已结束或即将结束）
        let paths: Vec<String> = STATE
            .lock()
            .unwrap()
            .sources
            .iter()
            .map(|s| s.parse_path.clone())
            .filter(|p| !p.is_empty())
            .collect();
        if !paths.is_empty() {
            let handle = app.clone();
            std::thread::spawn(move || {
                crate::shell_thread::run_on_shell(move || {
                    for p in paths {
                        crate::shell_menu::invoke_verb_direct(vec![p], None, "pintohome");
                    }
                });
                let _ = handle.emit("fs-quick-access-changed", ());
            });
        }
        return;
    }
    // 转发给目标文件夹自身的 IDropTarget → 完全原生语义（冲突对话框/进度/撤销）
    let Ok(item) = crate::shell_items::item_from_path(&t.parse_path) else {
        return;
    };
    let Ok(dt) = item.BindToHandler::<_, IDropTarget>(None, &BHID_SFUIObject) else {
        return;
    };
    let mut e = eff;
    if dt.DragEnter(dobj, keys, pt, &mut e).is_ok() && e != DROPEFFECT_NONE {
        let mut e2 = eff;
        let _ = dt.Drop(dobj, keys, pt, &mut e2);
    } else {
        let _ = dt.DragLeave();
    }
}

/* ===================== effect 规则（与资源管理器一致） ===================== */

fn compute_effect(keys: u32, st: &DragState) -> DROPEFFECT {
    let Some(t) = &st.target else {
        return DROPEFFECT_NONE;
    };
    let allowed = DROPEFFECT(if st.allowed == 0 { u32::MAX } else { st.allowed });
    if t.kind == "pin" {
        // 快速访问只能固定文件夹（与资源管理器一致）
        if st.sources.is_empty() || !st.sources.iter().all(|s| s.is_dir) {
            return DROPEFFECT_NONE;
        }
        return pick(DROPEFFECT_LINK, allowed);
    }
    // 拖到自身 / 拖进自己的子孙目录 → 禁止
    for s in &st.sources {
        if !s.parse_path.is_empty()
            && (eq_ci(&s.parse_path, &t.parse_path) || is_descendant(&t.parse_path, &s.parse_path))
        {
            return DROPEFFECT_NONE;
        }
        if let (Some(sf), Some(tf)) = (&s.fs_path, &t.fs_path) {
            if eq_ci(sf, tf) || is_descendant(tf, sf) {
                return DROPEFFECT_NONE;
            }
        }
    }
    let ctrl = keys & 0x8 != 0;
    let shift = keys & 0x4 != 0;
    let alt = keys & 0x20 != 0;
    let mut eff = if alt || (ctrl && shift) {
        DROPEFFECT_LINK
    } else if ctrl {
        DROPEFFECT_COPY
    } else if shift {
        DROPEFFECT_MOVE
    } else if st.sources.is_empty() {
        // 无法提取路径的外部源（Outlook 附件等）→ 复制
        DROPEFFECT_COPY
    } else if st.sources.iter().any(is_drive_root) {
        // 拖驱动器 → 创建快捷方式
        DROPEFFECT_LINK
    } else if st.sources.iter().any(|s| s.fs_path.is_none()) || t.is_container_file {
        // 虚拟/zip 内源，或目标是 zip 等文件型容器 → 复制
        DROPEFFECT_COPY
    } else if t
        .fs_path
        .as_deref()
        .map(|tf| {
            st.sources
                .iter()
                .all(|s| s.fs_path.as_deref().map(|sf| same_volume(sf, tf)).unwrap_or(false))
        })
        .unwrap_or(false)
    {
        DROPEFFECT_MOVE
    } else {
        DROPEFFECT_COPY
    };
    // zip 等文件型容器不支持移入 → 复制（Shift 亦然，与资源管理器一致）
    if eff == DROPEFFECT_MOVE && t.is_container_file {
        eff = DROPEFFECT_COPY;
    }
    // 移动到源自身所在目录 = 无操作
    if eff == DROPEFFECT_MOVE {
        if let Some(tf) = &t.fs_path {
            let all_same_parent = !st.sources.is_empty()
                && st.sources.iter().all(|s| {
                    s.fs_path
                        .as_deref()
                        .map(|p| parent_eq(p, tf))
                        .unwrap_or(false)
                });
            if all_same_parent {
                return DROPEFFECT_NONE;
            }
        }
    }
    pick(eff, allowed)
}

fn pick(want: DROPEFFECT, allowed: DROPEFFECT) -> DROPEFFECT {
    if (want & allowed) != DROPEFFECT_NONE {
        return want;
    }
    for e in [DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_LINK] {
        if (e & allowed) != DROPEFFECT_NONE {
            return e;
        }
    }
    DROPEFFECT_NONE
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// child 是否位于 ancestor 内部（大小写不敏感 + '\' 边界）
/// 按字节比较，避免多字节路径（如中文）的 char 边界 panic
fn is_descendant(child: &str, ancestor: &str) -> bool {
    let anc = ancestor.trim_end_matches('\\').as_bytes();
    let ch = child.as_bytes();
    if anc.is_empty() || ch.len() <= anc.len() {
        return false;
    }
    ch[..anc.len()].eq_ignore_ascii_case(anc) && ch[anc.len()] == b'\\'
}

fn parent_eq(child: &str, parent: &str) -> bool {
    match child.trim_end_matches('\\').rfind('\\') {
        Some(i) => {
            let dir = &child[..i];
            let dir = if dir.len() == 2 && dir.ends_with(':') {
                // "C:" → "C:\"
                &child[..i + 1]
            } else {
                dir
            };
            eq_ci(dir, parent.trim_end_matches('\\')) || eq_ci(dir, parent)
        }
        None => false,
    }
}

fn is_drive_root(s: &SourceInfo) -> bool {
    let p = s.fs_path.as_deref().unwrap_or(&s.parse_path);
    let p = p.trim_end_matches('\\');
    p.len() == 2 && p.as_bytes()[1] == b':'
}

fn same_volume(a: &str, b: &str) -> bool {
    let va = a.as_bytes();
    let vb = b.as_bytes();
    va.len() >= 2 && vb.len() >= 2 && va[1] == b':' && vb[1] == b':'
        && va[0].eq_ignore_ascii_case(&vb[0])
}

/* ===================== DROPDESCRIPTION（"移动到 X"提示） ===================== */

fn desc_for(eff: DROPEFFECT) -> (DROPIMAGETYPE, String, String) {
    let st = STATE.lock().unwrap();
    let (name, is_pin) = st
        .target
        .as_ref()
        .map(|t| (t.name.clone(), t.kind == "pin"))
        .unwrap_or_default();
    if eff == DROPEFFECT_NONE || name.is_empty() {
        return (DROPIMAGE_INVALID, String::new(), String::new());
    }
    if is_pin {
        return (DROPIMAGE_LINK, "固定到 %1".into(), name);
    }
    match eff {
        DROPEFFECT_MOVE => (DROPIMAGE_MOVE, "移动到 %1".into(), name),
        DROPEFFECT_COPY => (DROPIMAGE_COPY, "复制到 %1".into(), name),
        DROPEFFECT_LINK => (DROPIMAGE_LINK, "在 %1 中创建链接".into(), name),
        _ => (DROPIMAGE_INVALID, String::new(), String::new()),
    }
}

unsafe fn set_drop_description(dobj: &IDataObject, img: DROPIMAGETYPE, msg: &str, insert: &str) {
    {
        let mut st = STATE.lock().unwrap();
        let cur = (img.0, msg.to_string(), insert.to_string());
        if st.last_desc.as_ref() == Some(&cur) {
            return;
        }
        st.last_desc = Some(cur);
    }
    let cf = RegisterClipboardFormatW(w!("DropDescription"));
    if cf == 0 {
        return;
    }
    let fmt = FORMATETC {
        cfFormat: cf as u16,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let Ok(hg) = GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of::<DROPDESCRIPTION>()) else {
        return;
    };
    let p = GlobalLock(hg) as *mut DROPDESCRIPTION;
    if p.is_null() {
        let _ = GlobalFree(Some(hg));
        return;
    }
    let mut dd = DROPDESCRIPTION::default();
    dd.r#type = img;
    dd.szMessage = wstr260(msg);
    dd.szInsert = wstr260(insert);
    p.write_unaligned(dd);
    let _ = GlobalUnlock(hg);
    let medium = STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: hg },
        pUnkForRelease: ManuallyDrop::new(None),
    };
    if dobj.SetData(&fmt, &medium, true).is_err() {
        let _ = GlobalFree(Some(hg));
    }
}

fn wstr260(s: &str) -> [u16; 260] {
    let mut dst = [0u16; 260];
    let mut i = 0;
    for u in s.encode_utf16() {
        if i >= 259 {
            break;
        }
        dst[i] = u;
        i += 1;
    }
    dst
}

/* ===================== 源提取 ===================== */

unsafe fn name_of(item: &IShellItem, sigdn: SIGDN) -> Option<String> {
    let pw = item.GetDisplayName(sigdn).ok()?;
    let s = pw.to_string().ok();
    CoTaskMemFree(Some(pw.0 as *const c_void));
    s.filter(|x| !x.is_empty())
}

unsafe fn sources_from_array(arr: &IShellItemArray) -> Vec<SourceInfo> {
    let mut v = Vec::new();
    let count = arr.GetCount().unwrap_or(0);
    for i in 0..count {
        if let Ok(item) = arr.GetItemAt(i) {
            let fs_path = name_of(&item, SIGDN_FILESYSPATH);
            let is_dir = fs_path
                .as_deref()
                .map(|p| std::path::Path::new(p).is_dir())
                .unwrap_or(false);
            v.push(SourceInfo {
                parse_path: name_of(&item, SIGDN_DESKTOPABSOLUTEPARSING).unwrap_or_default(),
                fs_path,
                is_dir,
            });
        }
    }
    v
}

unsafe fn extract_sources(dobj: &IDataObject) -> Vec<SourceInfo> {
    if let Ok(arr) = SHCreateShellItemArrayFromDataObject::<_, IShellItemArray>(dobj) {
        let v = sources_from_array(&arr);
        if !v.is_empty() {
            return v;
        }
    }
    // CF_HDROP 回退（浏览器等非 shell 源）
    let fmt = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let mut v = Vec::new();
    if let Ok(mut medium) = dobj.GetData(&fmt) {
        let hdrop = HDROP(medium.u.hGlobal.0 as _);
        let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(hdrop, i, Some(&mut buf));
            let path = String::from_utf16_lossy(&buf[..len]);
            if !path.is_empty() {
                let is_dir = std::path::Path::new(&path).is_dir();
                v.push(SourceInfo {
                    parse_path: path.clone(),
                    fs_path: Some(path),
                    is_dir,
                });
            }
        }
        ReleaseStgMedium(&mut medium);
    }
    v
}
