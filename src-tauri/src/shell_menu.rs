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

/// 由解析路径集合构建 IShellItemArray（右键菜单/现代命令共用）
pub(crate) fn selection_array(paths: &[String]) -> windows::core::Result<IShellItemArray> {
    let mut pidls = Vec::new();
    for p in paths {
        pidls.push(parse_to_pidl(p)?);
    }
    let guard = PidlGuard(pidls);
    let const_pidls: Vec<*const ITEMIDLIST> = guard.0.iter().map(|p| *p as *const _).collect();
    unsafe { SHCreateShellItemArrayFromIDLists(&const_pidls) }
}

fn context_menu_for_items(paths: &[String]) -> windows::core::Result<IContextMenu> {
    let array = selection_array(paths)?;
    unsafe { array.BindToHandler(None, &windows::Win32::UI::Shell::BHID_SFUIObject) }
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
    invoke_menu_verb(&menu_obj, verb)
}

/// QueryContextMenu 后按 verb 字符串 InvokeCommand
fn invoke_menu_verb(menu_obj: &IContextMenu, verb: &str) -> bool {
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

/// 快速访问项上的 verb（unpinfromhome/removefromhome）：
/// 这类 verb 只在快速访问命名空间枚举出的子项上生效，
/// 对按文件系统路径重新解析的项执行会被 shell 静默忽略（pintohome 无此限制）
pub fn quick_access_verb(path: &str, verb: &str) -> bool {
    use crate::shell_items::{pwstr_to_string_free, QUICK_ACCESS};
    use windows::Win32::UI::Shell::{
        BHID_EnumItems, IEnumShellItems, SIGDN_DESKTOPABSOLUTEPARSING,
    };
    let Ok(folder) = item_from_path(QUICK_ACCESS) else {
        return false;
    };
    unsafe {
        let Ok(enumerator) = folder.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems)
        else {
            return false;
        };
        loop {
            let mut fetched = 0u32;
            let mut items: [Option<windows::Win32::UI::Shell::IShellItem>; 16] =
                std::array::from_fn(|_| None);
            let _ = enumerator.Next(&mut items, Some(&mut fetched));
            if fetched == 0 {
                return false;
            }
            for it in items.iter().take(fetched as usize).flatten() {
                let parse = it
                    .GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING)
                    .map(pwstr_to_string_free)
                    .unwrap_or_default();
                if parse != path {
                    continue;
                }
                let Ok(menu_obj) = it.BindToHandler::<_, IContextMenu>(
                    None,
                    &windows::Win32::UI::Shell::BHID_SFUIObject,
                ) else {
                    return false;
                };
                return invoke_menu_verb(&menu_obj, verb);
            }
        }
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

/// 删除（IFileOperation）：permanent=false 进回收站；true 为 Shift+Delete 永久删除
/// （不带 FOF_ALLOWUNDO，系统会弹出"永久删除"确认对话框）
pub fn delete_items(paths: Vec<String>, permanent: bool) -> Result<(), String> {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{FileOperation, IFileOperation, FOF_ALLOWUNDO};
    unsafe {
        let op: IFileOperation =
            CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER).map_err(|e| e.message())?;
        if !permanent {
            op.SetOperationFlags(FOF_ALLOWUNDO).map_err(|e| e.message())?;
        }
        for p in &paths {
            let item = item_from_path(p).map_err(|e| e.message())?;
            op.DeleteItem(&item, None).map_err(|e| e.message())?;
        }
        op.PerformOperations().map_err(|e| e.message())?;
        Ok(())
    }
}

/// 压缩为 ZIP 文件：资源管理器同款 IExplorerCommand（Windows.CompressTo.Zip），
/// 对选中项生成同目录 zip（与右键"压缩为 ZIP 文件"一致）
pub fn compress_to_zip(selection: Vec<String>) -> bool {
    use windows::Win32::System::Com::{CoCreateInstance, IBindCtx, CLSCTX_ALL};
    use windows::Win32::UI::Shell::IExplorerCommand;
    const CLSID_COMPRESS_ZIP: windows::core::GUID =
        windows::core::GUID::from_u128(0x9c07355e_c50a_45d2_b4a3_0a8235f8047f);
    let Ok(array) = selection_array(&selection) else {
        return false;
    };
    unsafe {
        match CoCreateInstance::<_, IExplorerCommand>(&CLSID_COMPRESS_ZIP, None, CLSCTX_ALL) {
            Ok(cmd) => cmd.Invoke(&array, None::<&IBindCtx>).is_ok(),
            Err(_) => false,
        }
    }
}

/// 将文本写入系统剪贴板（CF_UNICODETEXT）。前端 navigator.clipboard 在
/// Tauri webview 非安全上下文下不可靠，改走后端 Win32 确保生效。须在 STA 线程调用
pub fn set_clipboard_text(text: &str) -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(Some(helper_hwnd())).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let bytes = wide.len() * std::mem::size_of::<u16>();
        let ok = match GlobalAlloc(GMEM_MOVEABLE, bytes) {
            Ok(hglobal) => {
                let ptr = GlobalLock(hglobal) as *mut u16;
                if ptr.is_null() {
                    false
                } else {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                    let _ = GlobalUnlock(hglobal);
                    // SetClipboardData 成功后剪贴板接管 hglobal，不再手动释放
                    SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hglobal.0))).is_ok()
                }
            }
            Err(_) => false,
        };
        let _ = CloseClipboard();
        ok
    }
}

/// 添加到收藏夹：资源管理器同款 IExplorerCommand（Windows.AddToFavorites，文件专用）
pub fn add_to_favorites(selection: Vec<String>) -> bool {
    use windows::Win32::System::Com::{CoCreateInstance, IBindCtx, CLSCTX_ALL};
    use windows::Win32::UI::Shell::IExplorerCommand;
    const CLSID_ADD_FAVORITES: windows::core::GUID =
        windows::core::GUID::from_u128(0x323ca680_c24d_4099_b94d_446dd2d7249e);
    let Ok(array) = selection_array(&selection) else {
        return false;
    };
    unsafe {
        match CoCreateInstance::<_, IExplorerCommand>(&CLSID_ADD_FAVORITES, None, CLSCTX_ALL) {
            Ok(cmd) => cmd.Invoke(&array, None::<&IBindCtx>).is_ok(),
            Err(_) => false,
        }
    }
}

/// "更多"菜单里的系统动作（白名单）：映射/断开网络驱动器、添加网络位置、文件夹选项、
/// 磁盘清理/优化驱动器、连接到媒体服务器
pub fn system_action(action: &str) -> bool {
    use windows::core::w;
    use windows::Win32::UI::Shell::ShellExecuteW;
    // 磁盘清理：cleanmgr（"clean-drive:C" 指定盘符；无盘符时弹系统选盘对话框）
    if let Some(rest) = action.strip_prefix("clean-drive") {
        let args: Option<Vec<u16>> = rest
            .strip_prefix(':')
            .map(|l| crate::shell_items::to_wide(&format!("/d {l}:")));
        unsafe {
            let h = ShellExecuteW(
                None,
                w!("open"),
                w!("cleanmgr.exe"),
                args.as_ref().map_or(PCWSTR::null(), |a| PCWSTR(a.as_ptr())),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
            return h.0 as usize > 32;
        }
    }
    // 优化驱动器：系统"碎片整理和优化驱动器"对话框（与资源管理器 Windows.Defragment 一致）
    if action == "optimize-drives" {
        unsafe {
            let h = ShellExecuteW(
                None,
                w!("open"),
                w!("dfrgui.exe"),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
            return h.0 as usize > 32;
        }
    }
    // 连接到媒体服务器：资源管理器同款 IExplorerCommand（CommandStore Windows.AddMediaServer）
    if action == "add-media-server" {
        use windows::Win32::System::Com::{CoCreateInstance, IBindCtx, CLSCTX_ALL};
        use windows::Win32::UI::Shell::IExplorerCommand;
        const CLSID_ADD_MEDIA_SERVER: windows::core::GUID =
            windows::core::GUID::from_u128(0xd7bfd8f3_678c_4827_b84b_0e5fc6d15be3);
        unsafe {
            if let Ok(cmd) =
                CoCreateInstance::<_, IExplorerCommand>(&CLSID_ADD_MEDIA_SERVER, None, CLSCTX_ALL)
            {
                return cmd
                    .Invoke(None::<&IShellItemArray>, None::<&IBindCtx>)
                    .is_ok();
            }
        }
        return false;
    }
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

/* ===================== Fluent 右键菜单数据源（前端 HTML 渲染） ===================== */

thread_local! {
    // 挂起的经典菜单实例：前端展示后，用户选择时在同一 IContextMenu 上 InvokeCommand
    static PENDING_CTX: RefCell<Option<(IContextMenu, HMENU, Vec<String>)>> = const { RefCell::new(None) };
}

#[derive(Serialize)]
pub struct CtxNode {
    pub id: u32,
    pub label: String,
    pub accel: String,
    pub verb: String,
    pub icon: Option<String>,
    pub separator: bool,
    pub children: Vec<CtxNode>,
}

fn clear_pending_ctx() {
    PENDING_CTX.with(|p| {
        if let Some((_, hmenu, _)) = p.borrow_mut().take() {
            unsafe { let _ = DestroyMenu(hmenu); }
        }
    });
}

/// 递归枚举菜单树（子菜单先转发 WM_INITMENUPOPUP 触发延迟填充）
unsafe fn enum_menu_tree(menu_obj: &IContextMenu, hmenu: HMENU, depth: u32) -> Vec<CtxNode> {
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
    let n = GetMenuItemCount(Some(hmenu)).max(0) as u32;
    let mut out: Vec<CtxNode> = Vec::new();
    for i in 0..n {
        let hsub = GetSubMenu(hmenu, i as i32);
        let id = GetMenuItemID(hmenu, i as i32);
        let raw = menu_item_text(hmenu, i);
        if hsub.0.is_null() && (id == 0 || id == u32::MAX) {
            // 分隔线（去重：不连续、不开头）
            if !out.is_empty() && !out.last().map(|e| e.separator).unwrap_or(false) {
                out.push(CtxNode {
                    id: 0, label: String::new(), accel: String::new(), verb: String::new(),
                    icon: None, separator: true, children: Vec::new(),
                });
            }
            continue;
        }
        if raw.is_empty() {
            continue;
        }
        let (label, accel) = match raw.split_once('\t') {
            Some((l, a)) => (l.to_string(), a.to_string()),
            None => (raw, String::new()),
        };
        let mut children = Vec::new();
        if !hsub.0.is_null() && depth < 3 {
            // 性能：先直接枚举，仅当子菜单为空（延迟填充型）才转发 WM_INITMENUPOPUP
            children = enum_menu_tree(menu_obj, hsub, depth + 1);
            if children.is_empty() {
                let _ = SendMessageW(
                    helper_hwnd(),
                    WM_INITMENUPOPUP,
                    Some(windows::Win32::Foundation::WPARAM(hsub.0 as usize)),
                    Some(windows::Win32::Foundation::LPARAM(i as isize)),
                );
                children = enum_menu_tree(menu_obj, hsub, depth + 1);
            }
        }
        let verb = if (ID_FIRST..=ID_LAST).contains(&id) {
            get_verb(menu_obj, id - ID_FIRST)
        } else {
            String::new()
        };
        out.push(CtxNode {
            id,
            label,
            accel,
            verb,
            icon: menu_item_icon(hmenu, i),
            separator: false,
            children,
        });
    }
    while out.last().map(|e| e.separator).unwrap_or(false) {
        out.pop();
    }
    out
}

/// 获取完整经典菜单树（含第三方扩展/子菜单/图标），供前端 Fluent 风格渲染；
/// selection 为空且提供 background 时返回文件夹空白处的背景菜单树
pub fn get_ctx_menu(selection: Vec<String>, background: Option<String>) -> Vec<CtxNode> {
    clear_pending_ctx();
    let menu_obj = if selection.is_empty() {
        match background.as_deref().map(context_menu_for_background) {
            Some(Ok(m)) => m,
            _ => return Vec::new(),
        }
    } else {
        match context_menu_for_items(&selection) {
            Ok(m) => m,
            Err(_) => return Vec::new(),
        }
    };
    unsafe {
        let hmenu = match CreatePopupMenu() {
            Ok(h) => h,
            Err(_) => return Vec::new(),
        };
        if menu_obj
            .QueryContextMenu(hmenu, 0, ID_FIRST, ID_LAST, CMF_NORMAL | CMF_EXPLORE | CMF_CANRENAME)
            .is_err()
        {
            let _ = DestroyMenu(hmenu);
            return Vec::new();
        }
        use crate::shell_thread::{ACTIVE_MENU2, ACTIVE_MENU3};
        ACTIVE_MENU2.with(|m| *m.borrow_mut() = menu_obj.cast::<IContextMenu2>().ok());
        ACTIVE_MENU3.with(|m| *m.borrow_mut() = menu_obj.cast::<IContextMenu3>().ok());
        let tree = enum_menu_tree(&menu_obj, hmenu, 0);
        ACTIVE_MENU2.with(|m| *m.borrow_mut() = None);
        ACTIVE_MENU3.with(|m| *m.borrow_mut() = None);
        PENDING_CTX.with(|p| *p.borrow_mut() = Some((menu_obj, hmenu, selection)));
        tree
    }
}

/// 执行菜单项；rename/文件夹 open 交给前端处理（与原生弹菜单路径同逻辑）
pub fn invoke_ctx(id: u32) -> MenuResult {
    let none = MenuResult { action: "none".into(), verb: String::new() };
    let Some((menu_obj, hmenu, selection)) = PENDING_CTX.with(|p| p.borrow_mut().take()) else {
        return none;
    };
    unsafe {
        let offset = id - ID_FIRST;
        let verb = get_verb(&menu_obj, offset);
        if verb == "rename" {
            let _ = DestroyMenu(hmenu);
            return MenuResult { action: "rename".into(), verb };
        }
        if verb == "open" && selection.len() == 1 {
            if let Ok(item) = item_from_path(&selection[0]) {
                use windows::Win32::System::SystemServices::{SFGAO_FOLDER, SFGAO_STREAM};
                if let Ok(attrs) = item.GetAttributes(SFGAO_FOLDER | SFGAO_STREAM) {
                    if (attrs.0 & SFGAO_FOLDER.0) != 0 && (attrs.0 & SFGAO_STREAM.0) == 0 {
                        let _ = DestroyMenu(hmenu);
                        return MenuResult { action: "navigate".into(), verb };
                    }
                }
            }
        }
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
        MenuResult {
            action: if ok { "invoked".into() } else { "none".into() },
            verb,
        }
    }
}

/// 格式化驱动器：弹出系统格式化对话框（与资源管理器"格式化..."一致）。
/// 对话框是模态消息循环，在独立线程运行，避免阻塞 shell 线程
pub fn format_drive(letter: char) {
    let idx = (letter.to_ascii_uppercase() as u8).wrapping_sub(b'A') as u32;
    if idx >= 26 {
        return;
    }
    std::thread::spawn(move || unsafe {
        use windows::Win32::System::Com::{
            CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
        };
        use windows::Win32::UI::Shell::{SHFormatDrive, SHFMT_ID_DEFAULT, SHFMT_OPT};
        use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let _ = SHFormatDrive(GetDesktopWindow(), idx, SHFMT_ID_DEFAULT, SHFMT_OPT(0));
    });
}

/// 前端菜单关闭未选择时释放挂起实例
pub fn close_ctx() {
    clear_pending_ctx();
}

/// 剪贴板中是否有可粘贴的文件（背景菜单"粘贴"按钮可用性，与资源管理器一致）
pub fn clipboard_has_files() -> bool {
    use windows::core::w;
    use windows::Win32::System::DataExchange::{
        IsClipboardFormatAvailable, RegisterClipboardFormatW,
    };
    unsafe {
        // CF_HDROP = 15
        if IsClipboardFormatAvailable(15).is_ok() {
            return true;
        }
        let idlist = RegisterClipboardFormatW(w!("Shell IDList Array"));
        idlist != 0 && IsClipboardFormatAvailable(idlist).is_ok()
    }
}
