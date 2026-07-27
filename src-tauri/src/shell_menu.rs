// 原生 Shell 右键菜单：IContextMenu + TrackPopupMenu，与资源管理器一模一样
// （包含"打开方式"、"发送到"、第三方扩展如 Git、7-Zip 等）
use serde::Serialize;
use windows::core::{Interface, PCWSTR, PSTR};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IContextMenu, IContextMenu2, IContextMenu3, IShellFolder, IShellItemArray,
    SHCreateShellItemArrayFromIDLists, BHID_SFObject, CMF_CANRENAME, CMF_EXPLORE, CMF_NORMAL,
    CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, GCS_VERBW,
    SHELLEXECUTEINFOW, SEE_MASK_IDLIST, SEE_MASK_NOASYNC, ShellExecuteExW,
};
use windows::Win32::UI::Shell::ILFree;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuRadioItem, CreatePopupMenu, DestroyMenu, GetCursorPos, GetMenuItemCount,
    GetMenuItemID, GetMenuItemInfoW, GetMenuStringW, GetSubMenu, SetForegroundWindow,
    TrackPopupMenuEx, HMENU, MENUITEMINFOW, MF_BYCOMMAND, MF_BYPOSITION, MF_POPUP, MF_SEPARATOR,
    MF_STRING, MIIM_BITMAP, SW_SHOWNORMAL, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    WM_INITMENUPOPUP,
};

use crate::shell_items::{item_from_path, parse_to_pidl};
use crate::shell_thread::{helper_hwnd, ACTIVE_MENU2, ACTIVE_MENU3};

// windows crate 未导出，手动定义
const CMIC_MASK_UNICODE: u32 = 0x00004000;

#[derive(Serialize)]
pub struct MenuResult {
    /// "none" | "invoked" | "navigate" | "rename" | "set-view" | "set-sort" | "set-sort-dir" | "refresh"
    pub action: String,
    pub verb: String,
}

/// 前端当前视图状态（用于背景菜单勾选标记）
#[derive(serde::Deserialize, Clone)]
pub struct ViewState {
    pub view: String,     // "details" | "icons"
    pub sort_key: String, // "name" | "date" | "type" | "size"
    pub sort_asc: bool,
}

struct PidlGuard(Vec<*mut ITEMIDLIST>);
impl Drop for PidlGuard {
    fn drop(&mut self) {
        for p in &self.0 {
            unsafe { ILFree(Some(*p)) };
        }
    }
}

fn context_menu_for_items(paths: &[String]) -> windows::core::Result<IContextMenu> {
    let mut pidls = Vec::new();
    for p in paths {
        pidls.push(parse_to_pidl(p)?);
    }
    let guard = PidlGuard(pidls);
    let const_pidls: Vec<*const ITEMIDLIST> = guard.0.iter().map(|p| *p as *const _).collect();
    unsafe {
        let array: IShellItemArray = SHCreateShellItemArrayFromIDLists(&const_pidls)?;
        array.BindToHandler(None, &windows::Win32::UI::Shell::BHID_SFUIObject)
    }
}

fn context_menu_for_background(folder_path: &str) -> windows::core::Result<IContextMenu> {
    unsafe {
        let item = item_from_path(folder_path)?;
        let sf: IShellFolder = item.BindToHandler(None, &BHID_SFObject)?;
        sf.CreateViewObject(helper_hwnd())
    }
}

fn get_verb(menu: &IContextMenu, cmd: u32) -> String {
    unsafe {
        let mut buf = [0u16; 128];
        if menu
            .GetCommandString(cmd as usize, GCS_VERBW, None, PSTR(buf.as_mut_ptr() as *mut u8), buf.len() as u32)
            .is_ok()
        {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
            String::from_utf16_lossy(&buf[..end])
        } else {
            String::new()
        }
    }
}

const ID_FIRST: u32 = 1;
const ID_LAST: u32 = 0x7FFF;

// 自定义菜单项 ID（在 shell 命令范围之外）
const ID_VIEW_BASE: u32 = 0x9000;
// 视图模式顺序与 Explorer 一致
const VIEW_MODES: [(&str, &str); 8] = [
    ("xl-icons", "超大图标"),
    ("l-icons", "大图标"),
    ("m-icons", "中图标"),
    ("s-icons", "小图标"),
    ("list", "列表"),
    ("details", "详细信息"),
    ("tiles", "平铺"),
    ("content", "内容"),
];
const ID_SORT_NAME: u32 = 0x9010;
const ID_SORT_DATE: u32 = 0x9011;
const ID_SORT_TYPE: u32 = 0x9012;
const ID_SORT_SIZE: u32 = 0x9013;
const ID_SORT_ASC: u32 = 0x9014;
const ID_SORT_DESC: u32 = 0x9015;
const ID_REFRESH: u32 = 0x9020;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 向背景菜单头部注入 查看/排序方式/刷新（与资源管理器 DefView 一致）
unsafe fn inject_view_items(hmenu: HMENU, state: &ViewState) {
    let view_menu = CreatePopupMenu().unwrap_or_default();
    for (i, (key, label)) in VIEW_MODES.iter().enumerate() {
        let w = wide(label);
        let _ = AppendMenuW(view_menu, MF_STRING, (ID_VIEW_BASE + i as u32) as usize, PCWSTR(w.as_ptr()));
        let _ = key;
    }
    let checked_idx = VIEW_MODES
        .iter()
        .position(|(k, _)| *k == state.view)
        .unwrap_or(5) as u32;
    let _ = CheckMenuRadioItem(
        view_menu,
        ID_VIEW_BASE,
        ID_VIEW_BASE + VIEW_MODES.len() as u32 - 1,
        ID_VIEW_BASE + checked_idx,
        MF_BYCOMMAND.0,
    );

    let sort_menu = CreatePopupMenu().unwrap_or_default();
    for (id, label) in [
        (ID_SORT_NAME, "名称(N)"),
        (ID_SORT_DATE, "修改日期(M)"),
        (ID_SORT_TYPE, "类型(T)"),
        (ID_SORT_SIZE, "大小(S)"),
    ] {
        let w = wide(label);
        let _ = AppendMenuW(sort_menu, MF_STRING, id as usize, PCWSTR(w.as_ptr()));
    }
    let checked_sort = match state.sort_key.as_str() {
        "date" => ID_SORT_DATE,
        "type" => ID_SORT_TYPE,
        "size" => ID_SORT_SIZE,
        _ => ID_SORT_NAME,
    };
    let _ = CheckMenuRadioItem(sort_menu, ID_SORT_NAME, ID_SORT_SIZE, checked_sort, MF_BYCOMMAND.0);
    let _ = AppendMenuW(sort_menu, MF_SEPARATOR, 0, PCWSTR::null());
    let w_asc = wide("递增(A)");
    let w_desc = wide("递减(D)");
    let _ = AppendMenuW(sort_menu, MF_STRING, ID_SORT_ASC as usize, PCWSTR(w_asc.as_ptr()));
    let _ = AppendMenuW(sort_menu, MF_STRING, ID_SORT_DESC as usize, PCWSTR(w_desc.as_ptr()));
    let checked_dir = if state.sort_asc { ID_SORT_ASC } else { ID_SORT_DESC };
    let _ = CheckMenuRadioItem(sort_menu, ID_SORT_ASC, ID_SORT_DESC, checked_dir, MF_BYCOMMAND.0);

    let w_view = wide("查看(V)");
    let w_sort = wide("排序方式(O)");
    let w_refresh = wide("刷新(E)");
    let _ = AppendMenuW(hmenu, MF_POPUP, view_menu.0 as usize, PCWSTR(w_view.as_ptr()));
    let _ = AppendMenuW(hmenu, MF_POPUP, sort_menu.0 as usize, PCWSTR(w_sort.as_ptr()));
    let _ = AppendMenuW(hmenu, MF_STRING, ID_REFRESH as usize, PCWSTR(w_refresh.as_ptr()));
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
}

/// 自定义 ID → MenuResult
fn custom_result(cmd: u32) -> Option<MenuResult> {
    if (ID_VIEW_BASE..ID_VIEW_BASE + VIEW_MODES.len() as u32).contains(&cmd) {
        let key = VIEW_MODES[(cmd - ID_VIEW_BASE) as usize].0;
        return Some(MenuResult { action: "set-view".into(), verb: key.into() });
    }
    let (action, verb) = match cmd {
        ID_SORT_NAME => ("set-sort", "name"),
        ID_SORT_DATE => ("set-sort", "date"),
        ID_SORT_TYPE => ("set-sort", "type"),
        ID_SORT_SIZE => ("set-sort", "size"),
        ID_SORT_ASC => ("set-sort-dir", "asc"),
        ID_SORT_DESC => ("set-sort-dir", "desc"),
        ID_REFRESH => ("refresh", ""),
        _ => return None,
    };
    Some(MenuResult { action: action.into(), verb: verb.into() })
}

/// 弹出原生右键菜单。selection 为空时显示文件夹空白处背景菜单。
/// 必须在 STA 线程调用。
pub fn show_menu(selection: Vec<String>, background: Option<String>, state: Option<ViewState>) -> MenuResult {
    let none = MenuResult { action: "none".into(), verb: String::new() };
    let is_background = selection.is_empty();

    let menu_obj = if is_background {
        match background.as_deref().map(context_menu_for_background) {
            Some(Ok(m)) => m,
            _ => return none,
        }
    } else {
        match context_menu_for_items(&selection) {
            Ok(m) => m,
            Err(_) => return none,
        }
    };

    unsafe {
        let hmenu = match CreatePopupMenu() {
            Ok(h) => h,
            Err(_) => return none,
        };

        // 背景菜单：头部注入 查看/排序方式/刷新（与资源管理器一致）
        if is_background {
            let st = state.clone().unwrap_or(ViewState {
                view: "details".into(),
                sort_key: "name".into(),
                sort_asc: true,
            });
            inject_view_items(hmenu, &st);
        }
        let insert_at = GetMenuItemCount(Some(hmenu)).max(0) as u32;

        let flags = CMF_NORMAL | CMF_EXPLORE | CMF_CANRENAME;
        if menu_obj
            .QueryContextMenu(hmenu, insert_at, ID_FIRST, ID_LAST, flags)
            .is_err()
        {
            let _ = DestroyMenu(hmenu);
            return none;
        }

        // 注册消息转发（子菜单动态填充需要）
        ACTIVE_MENU2.with(|m| *m.borrow_mut() = menu_obj.cast::<IContextMenu2>().ok());
        ACTIVE_MENU3.with(|m| *m.borrow_mut() = menu_obj.cast::<IContextMenu3>().ok());

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let hwnd = helper_hwnd();
        let _ = SetForegroundWindow(hwnd);

        let cmd = TrackPopupMenuEx(
            hmenu,
            (TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD).0,
            pt.x,
            pt.y,
            hwnd,
            None,
        )
        .0 as u32;

        ACTIVE_MENU2.with(|m| *m.borrow_mut() = None);
        ACTIVE_MENU3.with(|m| *m.borrow_mut() = None);
        let _ = DestroyMenu(hmenu);

        if cmd < ID_FIRST {
            return none;
        }
        // 自定义项（查看/排序/刷新）
        if let Some(r) = custom_result(cmd) {
            return r;
        }
        let offset = cmd - ID_FIRST;
        let verb = get_verb(&menu_obj, offset);

        // 特殊 verb 交给前端处理
        if verb == "rename" {
            return MenuResult { action: "rename".into(), verb };
        }
        if verb == "open" && selection.len() == 1 {
            // 文件夹的"打开"由前端在应用内导航
            if let Ok(item) = item_from_path(&selection[0]) {
                use windows::Win32::System::SystemServices::{SFGAO_FOLDER, SFGAO_STREAM};
                if let Ok(attrs) = item.GetAttributes(SFGAO_FOLDER | SFGAO_STREAM) {
                    if (attrs.0 & SFGAO_FOLDER.0) != 0 && (attrs.0 & SFGAO_STREAM.0) == 0 {
                        return MenuResult { action: "navigate".into(), verb };
                    }
                }
            }
        }

        let mut info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: CMIC_MASK_UNICODE,
            hwnd,
            lpVerb: windows::core::PCSTR(offset as usize as *const u8),
            lpVerbW: PCWSTR(offset as usize as *const u16),
            nShow: SW_SHOWNORMAL.0,
            ptInvoke: pt,
            ..Default::default()
        };
        let ok = menu_obj
            .InvokeCommand(&mut info as *mut _ as *mut CMINVOKECOMMANDINFO)
            .is_ok();

        MenuResult {
            action: if ok { "invoked".into() } else { "none".into() },
            verb,
        }
    }
}

/// 不弹菜单，直接执行指定 verb（copy/cut/paste/delete/properties/link 等）
pub fn invoke_verb_direct(selection: Vec<String>, background: Option<String>, verb: &str) -> bool {
    let menu_obj = if selection.is_empty() {
        match background.as_deref().map(context_menu_for_background) {
            Some(Ok(m)) => m,
            _ => return false,
        }
    } else {
        match context_menu_for_items(&selection) {
            Ok(m) => m,
            Err(_) => return false,
        }
    };
    unsafe {
        let hmenu = match CreatePopupMenu() {
            Ok(h) => h,
            Err(_) => return false,
        };
        if menu_obj
            .QueryContextMenu(hmenu, 0, ID_FIRST, ID_LAST, CMF_NORMAL)
            .is_err()
        {
            let _ = DestroyMenu(hmenu);
            return false;
        }
        let verb_w: Vec<u16> = verb.encode_utf16().chain(std::iter::once(0)).collect();
        let verb_a = std::ffi::CString::new(verb).unwrap_or_default();
        let mut info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: CMIC_MASK_UNICODE,
            hwnd: helper_hwnd(),
            lpVerb: windows::core::PCSTR(verb_a.as_ptr() as *const u8),
            lpVerbW: PCWSTR(verb_w.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        let ok = menu_obj
            .InvokeCommand(&mut info as *mut _ as *mut CMINVOKECOMMANDINFO)
            .is_ok();
        let _ = DestroyMenu(hmenu);
        ok
    }
}

/// 打开项目（等价于资源管理器双击）：ShellExecuteEx + PIDL，支持虚拟项
pub fn open_item_default(path: &str) -> bool {
    unsafe {
        let pidl = match parse_to_pidl(path) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let mut sei = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_IDLIST | SEE_MASK_NOASYNC,
            lpIDList: pidl as *mut std::ffi::c_void,
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        let ok = ShellExecuteExW(&mut sei).is_ok();
        ILFree(Some(pidl));
        ok
    }
}

/// 重命名（IFileOperation，走系统冲突/确认对话框）
pub fn rename(path: &str, new_name: &str) -> Result<(), String> {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{FileOperation, IFileOperation, FOF_ALLOWUNDO};
    unsafe {
        let op: IFileOperation =
            CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER).map_err(|e| e.message())?;
        op.SetOperationFlags(FOF_ALLOWUNDO).map_err(|e| e.message())?;
        let item = item_from_path(path).map_err(|e| e.message())?;
        let wide: Vec<u16> = new_name.encode_utf16().chain(std::iter::once(0)).collect();
        op.RenameItem(&item, PCWSTR(wide.as_ptr()), None)
            .map_err(|e| e.message())?;
        op.PerformOperations().map_err(|e| e.message())?;
        Ok(())
    }
}

/// 新建文件夹
pub fn new_folder(parent: &str, name: &str) -> Result<(), String> {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{FileOperation, IFileOperation, FOF_ALLOWUNDO};
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
    unsafe {
        let op: IFileOperation =
            CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER).map_err(|e| e.message())?;
        op.SetOperationFlags(FOF_ALLOWUNDO).map_err(|e| e.message())?;
        let parent_item = item_from_path(parent).map_err(|e| e.message())?;
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        op.NewItem(
            &parent_item,
            FILE_ATTRIBUTE_DIRECTORY.0,
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            None,
        )
        .map_err(|e| e.message())?;
        op.PerformOperations().map_err(|e| e.message())?;
        Ok(())
    }
}

/// "更多"菜单里的系统动作（白名单）：映射/断开网络驱动器、添加网络位置、文件夹选项
pub fn system_action(action: &str) -> bool {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::core::w;
    let params: PCWSTR = match action {
        "map-drive" => w!("shell32.dll,SHHelpShortcuts_RunDLL Connect"),
        "disconnect-drive" => w!("shell32.dll,SHHelpShortcuts_RunDLL Disconnect"),
        "add-network-place" => w!("shwebsvc.dll,AddNetPlaceRunDll"),
        "folder-options" => w!("shell32.dll,Options_RunDLL 0"),
        _ => return false,
    };
    unsafe {
        let h = ShellExecuteW(
            None,
            w!("open"),
            w!("rundll32.exe"),
            params,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        h.0 as usize > 32
    }
}

/* ===================== 新建菜单（ShellNew，与资源管理器完全一致） ===================== */
use std::cell::RefCell;

thread_local! {
    // 挂起的新建菜单：前端展示列表后，用户选择时在同一 IContextMenu 实例上 InvokeCommand
    static PENDING_NEW: RefCell<Option<(IContextMenu, HMENU)>> = const { RefCell::new(None) };
}

#[derive(Serialize)]
pub struct NewMenuEntry {
    pub id: u32,
    pub label: String,
    pub icon: Option<String>,
    /// 分隔线项（保留原生菜单的分组，如"快捷方式"与模板之间）
    pub separator: bool,
}

fn clear_pending_new() {
    PENDING_NEW.with(|p| {
        if let Some((_, hmenu)) = p.borrow_mut().take() {
            unsafe { let _ = DestroyMenu(hmenu); }
        }
    });
}

fn menu_item_text(hmenu: HMENU, pos: u32) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let len = GetMenuStringW(hmenu, pos, Some(&mut buf), MF_BYPOSITION);
        if len <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..len as usize]).replace('&', "")
    }
}

fn menu_item_icon(hmenu: HMENU, pos: u32) -> Option<String> {
    unsafe {
        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_BITMAP,
            ..Default::default()
        };
        if GetMenuItemInfoW(hmenu, pos, true, &mut info).is_err() {
            return None;
        }
        // HBMMENU_* 魔数（小整数）不是真位图
        if info.hbmpItem.0.is_null() || (info.hbmpItem.0 as usize) <= 16 {
            return None;
        }
        crate::shell_icons::hbitmap_to_data_url(info.hbmpItem)
    }
}

/// 获取文件夹背景菜单的"新建"子菜单项（ShellNew 模板，含图标）
pub fn get_new_menu(folder: &str) -> Vec<NewMenuEntry> {
    clear_pending_new();
    let menu_obj = match context_menu_for_background(folder) {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    unsafe {
        let hmenu = match CreatePopupMenu() {
            Ok(h) => h,
            Err(_) => return Vec::new(),
        };
        if menu_obj
            .QueryContextMenu(hmenu, 0, ID_FIRST, ID_LAST, CMF_NORMAL)
            .is_err()
        {
            let _ = DestroyMenu(hmenu);
            return Vec::new();
        }

        // 定位"新建"子菜单
        let count = GetMenuItemCount(Some(hmenu)).max(0) as u32;
        for pos in 0..count {
            let hsub = GetSubMenu(hmenu, pos as i32);
            if hsub.0.is_null() {
                continue;
            }
            let text = menu_item_text(hmenu, pos);
            if !(text.contains("新建") || text.starts_with("New")) {
                continue;
            }
            // 触发延迟填充：ShellNew 列表在 WM_INITMENUPOPUP 时生成。
            // 复用隐藏窗口的消息转发链路（IContextMenu3::HandleMenuMsg2 优先），
            // 与 TrackPopupMenu 场景完全一致，否则 CLSID_NewMenu 可能不填充。
            use crate::shell_thread::{ACTIVE_MENU2, ACTIVE_MENU3};
            use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
            ACTIVE_MENU2.with(|m| *m.borrow_mut() = menu_obj.cast::<IContextMenu2>().ok());
            ACTIVE_MENU3.with(|m| *m.borrow_mut() = menu_obj.cast::<IContextMenu3>().ok());
            let _ = SendMessageW(
                helper_hwnd(),
                WM_INITMENUPOPUP,
                Some(windows::Win32::Foundation::WPARAM(hsub.0 as usize)),
                Some(windows::Win32::Foundation::LPARAM(pos as isize)),
            );
            ACTIVE_MENU2.with(|m| *m.borrow_mut() = None);
            ACTIVE_MENU3.with(|m| *m.borrow_mut() = None);

            let n = GetMenuItemCount(Some(hsub)).max(0) as u32;
            let mut out: Vec<NewMenuEntry> = Vec::new();
            for i in 0..n {
                let id = GetMenuItemID(hsub, i as i32);
                if id == 0 || id == u32::MAX {
                    // 分隔线：保留分组（去重：不连续、不开头）
                    if !out.is_empty() && !out.last().map(|e| e.separator).unwrap_or(false) {
                        out.push(NewMenuEntry { id: 0, label: String::new(), icon: None, separator: true });
                    }
                    continue;
                }
                let label = menu_item_text(hsub, i);
                if label.is_empty() {
                    continue;
                }
                out.push(NewMenuEntry {
                    id,
                    label,
                    icon: menu_item_icon(hsub, i),
                    separator: false,
                });
            }
            // 去掉末尾孤立分隔线
            while out.last().map(|e| e.separator).unwrap_or(false) {
                out.pop();
            }
            // 挂起菜单实例供后续 InvokeCommand
            PENDING_NEW.with(|p| *p.borrow_mut() = Some((menu_obj.clone(), hmenu)));
            return out;
        }
        let _ = DestroyMenu(hmenu);
        Vec::new()
    }
}

/// 执行新建菜单项（在挂起的同一菜单实例上）
pub fn invoke_new(id: u32) -> bool {
    let pending = PENDING_NEW.with(|p| p.borrow_mut().take());
    let Some((menu_obj, hmenu)) = pending else {
        return false;
    };
    unsafe {
        let offset = id - ID_FIRST;
        let mut info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: CMIC_MASK_UNICODE,
            hwnd: helper_hwnd(),
            lpVerb: windows::core::PCSTR(offset as usize as *const u8),
            lpVerbW: PCWSTR(offset as usize as *const u16),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        let ok = menu_obj
            .InvokeCommand(&mut info as *mut _ as *mut CMINVOKECOMMANDINFO)
            .is_ok();
        let _ = DestroyMenu(hmenu);
        ok
    }
}
