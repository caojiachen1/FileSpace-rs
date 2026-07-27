// Win11 现代右键菜单扩展枚举：
// 现代扩展（IExplorerCommand）注册在 Windows.FileExplorerContextMenus 契约下（MSIX/稀疏包），
// 从注册表枚举其 CLSID 并实例化，即可得到与资源管理器一级菜单完全一致的扩展列表
// （含标题/图标/GetState 可见性判定/子命令），这正是资源管理器自身的判定方式。
use std::cell::RefCell;

use serde::Serialize;
use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::System::Com::{CoCreateInstance, CLSIDFromString, CLSCTX_ALL};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER,
    HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::{
    IEnumExplorerCommand, IExplorerCommand, IShellItemArray, PathParseIconLocationW,
    SHDefExtractIconW, ECF_HASSUBCOMMANDS, ECS_HIDDEN,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};

use crate::shell_items::to_wide;

const CONTRACT_PATH: &str =
    "Software\\Classes\\Extensions\\ContractId\\Windows.FileExplorerContextMenus\\PackageId";

thread_local! {
    // 挂起的现代命令实例：菜单展示后按 mid 调 Invoke
    static PENDING_MODERN: RefCell<Option<(Vec<(u32, IExplorerCommand)>, IShellItemArray)>> =
        const { RefCell::new(None) };
    // 会话级实例缓存：CoCreateInstance（常为跨进程 COM 服务）是最大开销，
    // 首次构建后复用，后续右键只需 GetState/GetTitle
    static CMD_CACHE: RefCell<Option<Vec<IExplorerCommand>>> = const { RefCell::new(None) };
    // 图标定位串 -> PNG 缓存（提取 + 编码成本不低）
    static ICON_LOC_CACHE: RefCell<std::collections::HashMap<String, Option<String>>> =
        RefCell::new(std::collections::HashMap::new());
}

#[derive(Serialize)]
pub struct ModernNode {
    pub mid: u32,
    pub label: String,
    pub icon: Option<String>,
    pub children: Vec<ModernNode>,
}

fn reg_subkeys(root: HKEY, path: &str) -> Vec<String> {
    unsafe {
        let wide = to_wide(path);
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(root, PCWSTR(wide.as_ptr()), Some(0), KEY_READ, &mut hkey).is_err() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut index = 0u32;
        loop {
            let mut buf = [0u16; 512];
            let mut len = buf.len() as u32;
            let r = RegEnumKeyExW(
                hkey, index, Some(PWSTR(buf.as_mut_ptr())), &mut len, None, None, None, None,
            );
            if r.is_err() {
                break;
            }
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
            index += 1;
        }
        let _ = RegCloseKey(hkey);
        out
    }
}

fn reg_string(root: HKEY, path: &str, value: &str) -> Option<String> {
    unsafe {
        let wpath = to_wide(path);
        let wval = to_wide(value);
        let mut buf = vec![0u16; 512];
        let mut len = (buf.len() * 2) as u32;
        let r = RegGetValueW(
            root,
            PCWSTR(wpath.as_ptr()),
            PCWSTR(wval.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as _),
            Some(&mut len),
        );
        if r.is_err() {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }
}

/// 枚举 Windows.FileExplorerContextMenus 契约下注册的全部 IExplorerCommand CLSID
fn modern_clsids() -> Vec<GUID> {
    let mut out: Vec<GUID> = Vec::new();
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        for pkg in reg_subkeys(root, CONTRACT_PATH) {
            let base = format!("{CONTRACT_PATH}\\{pkg}\\ActivatableClassId");
            for class in reg_subkeys(root, &base) {
                let cp = format!("{base}\\{class}\\CustomProperties");
                if let Some(s) = reg_string(root, &cp, "Clsid") {
                    let w = to_wide(&s);
                    if let Ok(g) = unsafe { CLSIDFromString(PCWSTR(w.as_ptr())) } {
                        if !out.contains(&g) {
                            out.push(g);
                        }
                    }
                }
            }
        }
    }
    out
}

fn pwstr_free(p: PWSTR) -> String {
    unsafe {
        let s = if p.is_null() { String::new() } else { p.to_string().unwrap_or_default() };
        windows::Win32::System::Com::CoTaskMemFree(Some(p.as_ptr() as _));
        s
    }
}

/// 图标定位串（"path,-id"）→ PNG data URL（带缓存）；ms-resource 间接串跳过
fn icon_from_location(loc: &str) -> Option<String> {
    if loc.is_empty() || loc.starts_with('@') {
        return None;
    }
    if let Some(hit) = ICON_LOC_CACHE.with(|c| c.borrow().get(loc).cloned()) {
        return hit;
    }
    let png = icon_from_location_uncached(loc);
    ICON_LOC_CACHE.with(|c| c.borrow_mut().insert(loc.to_string(), png.clone()));
    png
}

fn icon_from_location_uncached(loc: &str) -> Option<String> {
    unsafe {
        let mut buf: Vec<u16> = loc.encode_utf16().collect();
        buf.resize(buf.len().max(4) + 260, 0);
        let index = PathParseIconLocationW(PWSTR(buf.as_mut_ptr()));
        let mut hicon = HICON::default();
        let hr = SHDefExtractIconW(PCWSTR(buf.as_ptr()), index, 0, Some(&mut hicon), None, 32);
        if hr.is_err() || hicon.is_invalid() {
            return None;
        }
        let png = hicon_to_png(hicon);
        let _ = DestroyIcon(hicon);
        png
    }
}

unsafe fn hicon_to_png(hicon: HICON) -> Option<String> {
    let mut info = ICONINFO::default();
    GetIconInfo(hicon, &mut info).ok()?;
    let result = if !info.hbmColor.is_invalid() {
        crate::shell_icons::hbitmap_to_data_url(info.hbmColor)
    } else {
        None
    };
    if !info.hbmColor.is_invalid() {
        let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
    }
    if !info.hbmMask.is_invalid() {
        let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
    }
    result
}

unsafe fn build_node(
    cmd: &IExplorerCommand,
    array: &IShellItemArray,
    store: &mut Vec<(u32, IExplorerCommand)>,
    next_id: &mut u32,
    depth: u32,
) -> Option<ModernNode> {
    // GetState 决定该扩展对当前选中项是否可见（资源管理器同款判定）；
    // fOkToBeSlow=false，E_PENDING 等错误按可见处理
    let state = cmd.GetState(array, false).unwrap_or(0);
    if state & (ECS_HIDDEN.0 as u32) != 0 {
        return None;
    }
    let title = cmd.GetTitle(array).ok().map(pwstr_free).unwrap_or_default();
    if title.trim().is_empty() {
        return None;
    }
    let icon = cmd
        .GetIcon(array)
        .ok()
        .map(pwstr_free)
        .and_then(|loc| icon_from_location(&loc));
    let flags = cmd.GetFlags().unwrap_or(0);
    let mut children = Vec::new();
    if flags & (ECF_HASSUBCOMMANDS.0 as u32) != 0 && depth < 2 {
        if let Ok(e) = cmd.EnumSubCommands() {
            enum_sub(&e, array, store, next_id, depth + 1, &mut children);
        }
    }
    let mid = *next_id;
    *next_id += 1;
    store.push((mid, cmd.clone()));
    Some(ModernNode { mid, label: title, icon, children })
}

unsafe fn enum_sub(
    e: &IEnumExplorerCommand,
    array: &IShellItemArray,
    store: &mut Vec<(u32, IExplorerCommand)>,
    next_id: &mut u32,
    depth: u32,
    out: &mut Vec<ModernNode>,
) {
    loop {
        let mut arr: [Option<IExplorerCommand>; 8] = Default::default();
        let mut fetched = 0u32;
        let _ = e.Next(&mut arr, Some(&mut fetched));
        if fetched == 0 {
            break;
        }
        for c in arr.iter().take(fetched as usize).flatten() {
            if let Some(n) = build_node(c, array, store, next_id, depth) {
                out.push(n);
            }
        }
    }
}

/// 获取当前选中项适用的现代菜单扩展（与 Win11 资源管理器一级菜单一致）
pub fn get_modern_menu(selection: Vec<String>) -> Vec<ModernNode> {
    clear();
    let Ok(array) = crate::shell_menu::selection_array(&selection) else {
        return Vec::new();
    };
    // 首次构建实例缓存，后续复用（跳过昂贵的 CoCreateInstance/进程启动）
    let cached = CMD_CACHE.with(|c| c.borrow().is_some());
    if !cached {
        let mut cmds = Vec::new();
        for clsid in modern_clsids() {
            if let Ok(cmd) = unsafe { CoCreateInstance::<_, IExplorerCommand>(&clsid, None, CLSCTX_ALL) } {
                cmds.push(cmd);
            }
        }
        CMD_CACHE.with(|c| *c.borrow_mut() = Some(cmds));
    }
    let cmds: Vec<IExplorerCommand> =
        CMD_CACHE.with(|c| c.borrow().as_ref().cloned().unwrap_or_default());
    let mut out = Vec::new();
    let mut store: Vec<(u32, IExplorerCommand)> = Vec::new();
    let mut next_id = 1u32;
    for cmd in &cmds {
        unsafe {
            if let Some(node) = build_node(cmd, &array, &mut store, &mut next_id, 0) {
                out.push(node);
            }
        }
    }
    PENDING_MODERN.with(|p| *p.borrow_mut() = Some((store, array)));
    out
}

/// 执行现代菜单项
pub fn invoke_modern(mid: u32) -> bool {
    let pending = PENDING_MODERN.with(|p| p.borrow_mut().take());
    let Some((store, array)) = pending else {
        return false;
    };
    let Some((_, cmd)) = store.into_iter().find(|(i, _)| *i == mid) else {
        return false;
    };
    unsafe { cmd.Invoke(&array, None).is_ok() }
}

pub fn clear() {
    PENDING_MODERN.with(|p| {
        p.borrow_mut().take();
    });
}
