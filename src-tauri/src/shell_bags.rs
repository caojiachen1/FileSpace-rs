// ShellBag 视图状态：通过 SHGetViewStatePropertyBag 读写每文件夹的视图模式，
// 与资源管理器共享同一份注册表存储（HKCU\...\Shell\BagMRU / Bags）。
// 资源管理器把视图设置存在 Bags\<n>\Shell\{文件夹模板GUID} 下的
// Mode / LogicalViewMode / IconSize 三个 REG_DWORD 值中。
use windows::core::PCWSTR;
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Variant::{VARIANT, VT_I4, VT_UI4};
use windows::Win32::System::WindowsProgramming::GetPrivateProfileStringW;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    ILFree, SHGetKnownFolderPath, SHGetViewStatePropertyBag, FOLDERID_Documents,
    FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures, FOLDERID_Videos, KF_FLAG_DEFAULT,
};

use crate::shell_items::{parse_to_pidl, pwstr_to_string_free, to_wide};

// SHGVSPB_* 标志（shlobj_core.h）
const SHGVSPB_PERUSER: u32 = 0x1;
const SHGVSPB_PERFOLDER: u32 = 0x4;
const SHGVSPB_NOAUTODEFAULTS: u32 = 0x8000_0000;
const SHGVSPB_FOLDER: u32 = SHGVSPB_PERUSER | SHGVSPB_PERFOLDER;
const SHGVSPB_FOLDERNODEFAULTS: u32 = SHGVSPB_FOLDER | SHGVSPB_NOAUTODEFAULTS;

// 文件夹模板 GUID（资源管理器按模板分别保存视图）
const FT_GENERIC: &str = "{5C4F28B5-F869-4E84-8E60-F11DB97C5CC7}";
const FT_DOCUMENTS: &str = "{7D49D726-3C21-4F05-99AA-FDC2C9474656}";
const FT_PICTURES: &str = "{B3690E58-E961-423B-B687-386EBFD83239}";
const FT_MUSIC: &str = "{94D6DDCC-4A68-4175-A374-BD584A510B78}";
const FT_VIDEOS: &str = "{5FA96407-7E77-483C-AC93-691D05850DE8}";
const FT_DOWNLOADS: &str = "{885A186E-A440-4ADA-812B-DB871B942259}";

const ALL_TEMPLATES: [&str; 6] = [
    FT_GENERIC, FT_DOWNLOADS, FT_DOCUMENTS, FT_PICTURES, FT_MUSIC, FT_VIDEOS,
];

/// FOLDERVIEWMODE + IconSize -> 前端 ViewMode 名称
fn mode_to_view(mode: u32, icon: u32) -> &'static str {
    match mode {
        4 => "details",
        3 => "list",
        6 => "tiles",
        8 => "content",
        2 => "s-icons", // FVM_SMALLICON
        _ => {
            // FVM_ICON / FVM_THUMBNAIL：按图标尺寸就近归类（16/48/96/256）
            if icon >= 128 {
                "xl-icons"
            } else if icon >= 72 {
                "l-icons"
            } else if icon >= 32 {
                "m-icons"
            } else {
                "s-icons"
            }
        }
    }
}

/// 前端 ViewMode -> (Mode, LogicalViewMode, IconSize)，与资源管理器写入的值一致
fn view_to_bag(view: &str) -> Option<(u32, u32, u32)> {
    Some(match view {
        "xl-icons" => (1, 3, 256),
        "l-icons" => (1, 3, 96),
        "m-icons" => (1, 3, 48),
        "s-icons" => (1, 3, 16),
        "list" => (3, 4, 16),
        "details" => (4, 1, 16),
        "tiles" => (6, 2, 48),
        "content" => (8, 5, 32),
        _ => return None,
    })
}

fn open_bag(pidl: *const ITEMIDLIST, template: &str, flags: u32) -> Option<IPropertyBag> {
    let name = to_wide(&format!("Shell\\{template}"));
    unsafe {
        SHGetViewStatePropertyBag::<PCWSTR, IPropertyBag>(Some(pidl), PCWSTR(name.as_ptr()), flags)
            .ok()
    }
}

fn read_u32(bag: &IPropertyBag, name: &str) -> Option<u32> {
    let wname = to_wide(name);
    unsafe {
        let mut var = VARIANT::default();
        // 预置 vt 提示属性包做类型转换
        (*var.Anonymous.Anonymous).vt = VT_UI4;
        bag.Read(PCWSTR(wname.as_ptr()), &mut var, None).ok()?;
        let vt = var.Anonymous.Anonymous.vt;
        if vt == VT_UI4 || vt == VT_I4 {
            Some(var.Anonymous.Anonymous.Anonymous.ulVal)
        } else {
            None
        }
    }
}

fn write_u32(bag: &IPropertyBag, name: &str, value: u32) -> bool {
    let wname = to_wide(name);
    unsafe {
        let mut var = VARIANT::default();
        (*var.Anonymous.Anonymous).vt = VT_UI4;
        (*var.Anonymous.Anonymous).Anonymous.ulVal = value;
        bag.Write(PCWSTR(wname.as_ptr()), &var).is_ok()
    }
}

fn known_folder_path(id: &windows::core::GUID) -> Option<String> {
    unsafe {
        SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)
            .ok()
            .map(pwstr_to_string_free)
    }
}

/// 读 desktop.ini 的 [ViewState] FolderType
fn desktop_ini_folder_type(path: &str) -> Option<String> {
    if path.starts_with("::") {
        return None;
    }
    let ini = to_wide(&format!("{}\\desktop.ini", path.trim_end_matches('\\')));
    let section = to_wide("ViewState");
    let key = to_wide("FolderType");
    let mut buf = [0u16; 64];
    let len = unsafe {
        GetPrivateProfileStringW(
            PCWSTR(section.as_ptr()),
            PCWSTR(key.as_ptr()),
            None,
            Some(&mut buf),
            PCWSTR(ini.as_ptr()),
        )
    };
    if len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// 推断资源管理器为该文件夹选用的模板 GUID（首选项）
fn primary_template(path: &str) -> &'static str {
    // 已知文件夹优先（下载/文档/图片/音乐/视频）
    let lower = path.trim_end_matches('\\').to_ascii_lowercase();
    let known: [(&windows::core::GUID, &'static str); 5] = [
        (&FOLDERID_Downloads, FT_DOWNLOADS),
        (&FOLDERID_Documents, FT_DOCUMENTS),
        (&FOLDERID_Pictures, FT_PICTURES),
        (&FOLDERID_Music, FT_MUSIC),
        (&FOLDERID_Videos, FT_VIDEOS),
    ];
    for (id, tpl) in known {
        if let Some(p) = known_folder_path(id) {
            if p.trim_end_matches('\\').to_ascii_lowercase() == lower {
                return tpl;
            }
        }
    }
    // desktop.ini 中的 FolderType
    if let Some(ft) = desktop_ini_folder_type(path) {
        return match ft.to_ascii_lowercase().as_str() {
            "documents" => FT_DOCUMENTS,
            "pictures" => FT_PICTURES,
            "music" => FT_MUSIC,
            "videos" => FT_VIDEOS,
            "downloads" => FT_DOWNLOADS,
            _ => FT_GENERIC,
        };
    }
    FT_GENERIC
}

/// 候选模板顺序：首选项在前，其余兜底（资源管理器可能按内容嗅探选了别的模板）
fn candidate_templates(path: &str) -> Vec<&'static str> {
    let primary = primary_template(path);
    let mut list = vec![primary];
    list.extend(ALL_TEMPLATES.iter().filter(|t| **t != primary));
    list
}

/// 查询文件夹已保存的视图模式（无记录返回 None，前端用默认视图）
pub fn view_mode_for(path: &str) -> Option<String> {
    let pidl = parse_to_pidl(path).ok()?;
    let result = candidate_templates(path).into_iter().find_map(|tpl| {
        let bag = open_bag(pidl, tpl, SHGVSPB_FOLDERNODEFAULTS)?;
        let mode = read_u32(&bag, "Mode")?;
        let icon = read_u32(&bag, "IconSize").unwrap_or(16);
        Some(mode_to_view(mode, icon).to_string())
    });
    unsafe { ILFree(Some(pidl)) };
    result
}

/// 把视图模式写回 ShellBag（资源管理器下次打开该文件夹即生效）
pub fn save_view_mode(path: &str, view: &str) -> bool {
    let Some((mode, lvm, icon)) = view_to_bag(view) else {
        return false;
    };
    let Ok(pidl) = parse_to_pidl(path) else {
        return false;
    };
    let candidates = candidate_templates(path);
    // 优先更新资源管理器已经写过的那个模板 bag，保持两边一致
    let target = candidates
        .iter()
        .find(|tpl| {
            open_bag(pidl, tpl, SHGVSPB_FOLDERNODEFAULTS)
                .and_then(|bag| read_u32(&bag, "Mode"))
                .is_some()
        })
        .copied()
        .unwrap_or(candidates[0]);
    let ok = match open_bag(pidl, target, SHGVSPB_FOLDER) {
        Some(bag) => {
            write_u32(&bag, "Mode", mode)
                && write_u32(&bag, "LogicalViewMode", lvm)
                && write_u32(&bag, "IconSize", icon)
        }
        None => false,
    };
    unsafe { ILFree(Some(pidl)) };
    ok
}
