// ShellBag 视图状态：通过 SHGetViewStatePropertyBag 读写每文件夹的视图模式，
// 与资源管理器共享同一份注册表存储（HKCU\...\Shell\BagMRU / Bags）。
// 资源管理器把视图设置存在 Bags\<n>\Shell\{文件夹模板GUID} 下的
// Mode / LogicalViewMode / IconSize 三个 REG_DWORD 值中。
use windows::core::{Interface, BSTR, GUID, PCWSTR};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::IStream;
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayUnaccessData,
};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows::Win32::System::Variant::{
    VariantClear, VARENUM, VARIANT, VT_ARRAY, VT_BSTR, VT_I4, VT_UI1, VT_UI4, VT_UNKNOWN,
};
use windows::Win32::System::WindowsProgramming::GetPrivateProfileStringW;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::PropertiesSystem::PSGetPropertyKeyFromName;
use windows::Win32::UI::Shell::{
    ILFree, SHCreateMemStream, SHGetKnownFolderPath, SHGetViewStatePropertyBag,
    FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures, FOLDERID_Videos,
    KF_FLAG_DEFAULT,
};

use crate::shell_items::{
    frontend_key_to_pkey, parse_to_pidl, pwstr_to_string_free, to_wide, GroupCols, SortColumn,
};

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

/* ===================== 排序 / 分组（与资源管理器共享 ShellBag） ===================== */

fn read_i32(bag: &IPropertyBag, name: &str) -> Option<i32> {
    let wname = to_wide(name);
    unsafe {
        let mut var = VARIANT::default();
        (*var.Anonymous.Anonymous).vt = VT_I4;
        bag.Read(PCWSTR(wname.as_ptr()), &mut var, None).ok()?;
        let vt = var.Anonymous.Anonymous.vt;
        if vt == VT_I4 || vt == VT_UI4 {
            Some(var.Anonymous.Anonymous.Anonymous.lVal)
        } else {
            None
        }
    }
}

fn read_bstr(bag: &IPropertyBag, name: &str) -> Option<String> {
    let wname = to_wide(name);
    unsafe {
        let mut var = VARIANT::default();
        (*var.Anonymous.Anonymous).vt = VT_BSTR;
        bag.Read(PCWSTR(wname.as_ptr()), &mut var, None).ok()?;
        let vt = var.Anonymous.Anonymous.vt;
        let s = if vt == VT_BSTR {
            Some((*var.Anonymous.Anonymous.Anonymous.bstrVal).to_string())
        } else {
            None
        };
        let _ = VariantClear(&mut var);
        s
    }
}

// ShellBag 的二进制值（如 Sort）由视图状态属性包以 VT_UNKNOWN(IStream) 形式暴露，
// 而非 VT_ARRAY|VT_UI1，读写都必须走 IStream
fn read_binary(bag: &IPropertyBag, name: &str) -> Option<Vec<u8>> {
    let wname = to_wide(name);
    unsafe {
        // 不预置 vt：视图状态属性包按存储类型返回（二进制值为 VT_UNKNOWN/IStream）
        let mut var = VARIANT::default();
        bag.Read(PCWSTR(wname.as_ptr()), &mut var, None).ok()?;
        let vt = var.Anonymous.Anonymous.vt;
        let mut out = None;
        if vt == VT_UNKNOWN {
            if let Some(unk) = var.Anonymous.Anonymous.Anonymous.punkVal.as_ref() {
                if let Ok(stream) = unk.cast::<IStream>() {
                    let mut data = Vec::new();
                    let mut buf = [0u8; 4096];
                    loop {
                        let mut got = 0u32;
                        let hr = stream.Read(
                            buf.as_mut_ptr() as *mut core::ffi::c_void,
                            buf.len() as u32,
                            Some(&mut got),
                        );
                        if hr.is_err() || got == 0 {
                            break;
                        }
                        data.extend_from_slice(&buf[..got as usize]);
                    }
                    if !data.is_empty() {
                        out = Some(data);
                    }
                }
            }
        } else if vt == VARENUM(VT_ARRAY.0 | VT_UI1.0) {
            let psa = var.Anonymous.Anonymous.Anonymous.parray;
            if !psa.is_null() {
                if let (Ok(lb), Ok(ub)) = (SafeArrayGetLBound(psa, 1), SafeArrayGetUBound(psa, 1)) {
                    let len = (ub - lb + 1).max(0) as usize;
                    let mut pdata = std::ptr::null_mut();
                    if SafeArrayAccessData(psa, &mut pdata).is_ok() {
                        out = Some(std::slice::from_raw_parts(pdata as *const u8, len).to_vec());
                        let _ = SafeArrayUnaccessData(psa);
                    }
                }
            }
        }
        let _ = VariantClear(&mut var);
        out
    }
}

fn write_i32(bag: &IPropertyBag, name: &str, value: i32) -> bool {
    let wname = to_wide(name);
    unsafe {
        let mut var = VARIANT::default();
        (*var.Anonymous.Anonymous).vt = VT_I4;
        (*var.Anonymous.Anonymous).Anonymous.lVal = value;
        bag.Write(PCWSTR(wname.as_ptr()), &var).is_ok()
    }
}

fn write_bstr(bag: &IPropertyBag, name: &str, value: &str) -> bool {
    let wname = to_wide(name);
    unsafe {
        let mut var = VARIANT::default();
        (*var.Anonymous.Anonymous).vt = VT_BSTR;
        (*var.Anonymous.Anonymous).Anonymous.bstrVal = core::mem::ManuallyDrop::new(BSTR::from(value));
        let ok = bag.Write(PCWSTR(wname.as_ptr()), &var).is_ok();
        let _ = VariantClear(&mut var);
        ok
    }
}

fn write_binary(bag: &IPropertyBag, name: &str, data: &[u8]) -> bool {
    let wname = to_wide(name);
    unsafe {
        let Some(stream) = SHCreateMemStream(Some(data)) else {
            return false;
        };
        let unk: windows::core::IUnknown = stream.into();
        let mut var = VARIANT::default();
        (*var.Anonymous.Anonymous).vt = VT_UNKNOWN;
        (*var.Anonymous.Anonymous).Anonymous.punkVal = core::mem::ManuallyDrop::new(Some(unk));
        let ok = bag.Write(PCWSTR(wname.as_ptr()), &var).is_ok();
        let _ = VariantClear(&mut var);
        ok
    }
}

fn bytes_to_guid(b: &[u8]) -> GUID {
    GUID {
        data1: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        data2: u16::from_le_bytes([b[4], b[5]]),
        data3: u16::from_le_bytes([b[6], b[7]]),
        data4: [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
    }
}

fn guid_to_bytes(g: &GUID) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&g.data1.to_le_bytes());
    out[4..6].copy_from_slice(&g.data2.to_le_bytes());
    out[6..8].copy_from_slice(&g.data3.to_le_bytes());
    out[8..16].copy_from_slice(&g.data4);
    out
}

fn guid_to_string(g: &GUID) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1, g.data2, g.data3,
        g.data4[0], g.data4[1], g.data4[2], g.data4[3],
        g.data4[4], g.data4[5], g.data4[6], g.data4[7]
    )
}

/// Sort 流内容：u32 列数 + 每列(16B GUID + u32 pid + u32 方向)。
/// 注册表 REG_BINARY 里另有 16B 零前缀（属性包持久化 IStream 时自加），流内容不含。
fn parse_sort_bytes(b: &[u8]) -> Option<Vec<SortColumn>> {
    let read_cols = |b: &[u8], off0: usize, count: usize| -> Vec<SortColumn> {
        let mut cols = Vec::new();
        let mut off = off0;
        for _ in 0..count {
            if off + 24 > b.len() {
                break;
            }
            let fmtid = bytes_to_guid(&b[off..off + 16]);
            let pid = u32::from_le_bytes(b[off + 16..off + 20].try_into().unwrap());
            let dir = u32::from_le_bytes(b[off + 20..off + 24].try_into().unwrap());
            cols.push(SortColumn { fmtid, pid, ascending: dir == 1 });
            off += 24;
        }
        cols
    };
    // 流布局：列数在偏移 0
    if b.len() >= 4 {
        let count = u32::from_le_bytes(b[0..4].try_into().ok()?) as usize;
        if count > 0 && 4 + count * 24 == b.len() {
            let cols = read_cols(b, 4, count);
            if !cols.is_empty() {
                return Some(cols);
            }
        }
    }
    // 注册表布局：16B 零头 + 列数在偏移 16
    if b.len() >= 20 {
        let count = u32::from_le_bytes(b[16..20].try_into().ok()?) as usize;
        if count > 0 && 20 + count * 24 == b.len() {
            let cols = read_cols(b, 20, count);
            if !cols.is_empty() {
                return Some(cols);
            }
        }
    }
    None
}

fn build_sort_bytes(cols: &[SortColumn]) -> Vec<u8> {
    // 写入流内容（无 16B 前缀，属性包持久化时自行添加）
    let mut v = Vec::with_capacity(4 + cols.len() * 24);
    v.extend_from_slice(&(cols.len() as u32).to_le_bytes());
    for c in cols {
        v.extend_from_slice(&guid_to_bytes(&c.fmtid));
        v.extend_from_slice(&c.pid.to_le_bytes());
        v.extend_from_slice(&(if c.ascending { 1u32 } else { 0xFFFF_FFFFu32 }).to_le_bytes());
    }
    v
}

fn read_sort(pidl: *const ITEMIDLIST, tpl: &str) -> Option<Vec<SortColumn>> {
    let bag = open_bag(pidl, tpl, SHGVSPB_FOLDERNODEFAULTS)?;
    let bytes = read_binary(&bag, "Sort")?;
    parse_sort_bytes(&bytes)
}

fn write_sort(pidl: *const ITEMIDLIST, tpl: &str, cols: &[SortColumn]) -> bool {
    match open_bag(pidl, tpl, SHGVSPB_FOLDER) {
        Some(bag) => write_binary(&bag, "Sort", &build_sort_bytes(cols)),
        None => false,
    }
}

/// 分组记录三态：None=bag 未记录；Some(None)=显式不分组；Some(Some)=分组列
fn read_group(pidl: *const ITEMIDLIST, tpl: &str) -> Option<Option<GroupCols>> {
    let bag = open_bag(pidl, tpl, SHGVSPB_FOLDERNODEFAULTS)?;
    let gv = read_u32(&bag, "GroupView")?;
    if gv == 0 {
        return Some(None);
    }
    let parse = || -> Option<GroupCols> {
        let fmtid = GUID::try_from(read_bstr(&bag, "GroupByKey:FMTID")?.as_str()).ok()?;
        let pid = read_u32(&bag, "GroupByKey:PID")?;
        let dir = read_i32(&bag, "GroupByDirection").unwrap_or(1);
        Some(GroupCols { fmtid, pid, ascending: dir >= 0 })
    };
    Some(parse())
}

fn write_group(pidl: *const ITEMIDLIST, tpl: &str, g: Option<GroupCols>) -> bool {
    let Some(bag) = open_bag(pidl, tpl, SHGVSPB_FOLDER) else {
        return false;
    };
    match g {
        None => write_u32(&bag, "GroupView", 0),
        Some(gc) => {
            write_u32(&bag, "GroupView", 1)
                && write_bstr(&bag, "GroupByKey:FMTID", &guid_to_string(&gc.fmtid))
                && write_u32(&bag, "GroupByKey:PID", gc.pid)
                && write_i32(&bag, "GroupByDirection", if gc.ascending { 1 } else { -1 })
        }
    }
}

/// 属性规范名 -> PROPERTYKEY
fn name_to_pkey(name: &str) -> Option<PROPERTYKEY> {
    let w = to_wide(name);
    let mut pk = PROPERTYKEY::default();
    unsafe { PSGetPropertyKeyFromName(PCWSTR(w.as_ptr()), &mut pk).ok()? };
    Some(pk)
}

/// 该属性未显式指定方向时的默认方向（日期/大小默认降序，其余升序）
fn default_dir_for(pk: &PROPERTYKEY) -> bool {
    !matches!(
        crate::shell_items::column_to_frontend_key(&pk.fmtid, pk.pid),
        Some("date") | Some("created") | Some("size")
    )
}

fn default_name_sort() -> Vec<SortColumn> {
    let pk = frontend_key_to_pkey("name").unwrap();
    vec![SortColumn { fmtid: pk.fmtid, pid: pk.pid, ascending: true }]
}

/// 解析单个 "prop:[-]System.Xxx" 记号 -> (PROPERTYKEY, 显式降序?)
fn parse_prop_name(token: &str) -> Option<(PROPERTYKEY, bool)> {
    let t = token.trim();
    let t = t.strip_prefix("prop:").unwrap_or(t);
    let desc = t.starts_with('-');
    let name = t.trim_start_matches('-').trim();
    if name.is_empty() || name.eq_ignore_ascii_case("System.Null") {
        return None;
    }
    Some((name_to_pkey(name)?, desc))
}

fn parse_sortbylist(s: &str) -> Option<Vec<SortColumn>> {
    let body = s.strip_prefix("prop:").unwrap_or(s);
    let mut cols = Vec::new();
    for tok in body.split(';') {
        if let Some((pk, desc)) = parse_prop_name(tok) {
            let ascending = if desc { false } else { default_dir_for(&pk) };
            cols.push(SortColumn { fmtid: pk.fmtid, pid: pk.pid, ascending });
        }
    }
    if cols.is_empty() {
        None
    } else {
        Some(cols)
    }
}

fn reg_read_sz(subkey: &str, value: &str) -> Option<String> {
    let sub = to_wide(subkey);
    let val = to_wide(value);
    unsafe {
        let mut size = 0u32;
        let r = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(sub.as_ptr()),
            PCWSTR(val.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        );
        if r.0 != 0 || size == 0 {
            return None;
        }
        let mut buf = vec![0u16; (size as usize) / 2 + 1];
        let mut size2 = (buf.len() * 2) as u32;
        let r = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(sub.as_ptr()),
            PCWSTR(val.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut size2),
        );
        if r.0 != 0 {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }
}

fn reg_read_dword(subkey: &str, value: &str) -> Option<u32> {
    let sub = to_wide(subkey);
    let val = to_wide(value);
    unsafe {
        let mut data = 0u32;
        let mut size = 4u32;
        let r = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(sub.as_ptr()),
            PCWSTR(val.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut core::ffi::c_void),
            Some(&mut size),
        );
        if r.0 == 0 {
            Some(data)
        } else {
            None
        }
    }
}

/// 读取资源管理器为该文件夹模板设定的默认排序/分组（HKLM FolderTypes TopViews）
fn template_default_sort(path: &str) -> (Vec<SortColumn>, Option<GroupCols>) {
    let tpl = primary_template(path);
    let base = format!(
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FolderTypes\\{}\\TopViews\\{{00000000-0000-0000-0000-000000000000}}",
        tpl
    );
    let sort = reg_read_sz(&base, "SortByList")
        .and_then(|s| parse_sortbylist(&s))
        .unwrap_or_else(default_name_sort);
    let group = reg_read_sz(&base, "GroupBy")
        .and_then(|s| parse_prop_name(&s))
        .map(|(pk, desc)| {
            let ascending = reg_read_dword(&base, "GroupAscending")
                .map(|d| d != 0)
                .unwrap_or(if desc { false } else { default_dir_for(&pk) });
            GroupCols { fmtid: pk.fmtid, pid: pk.pid, ascending }
        });
    (sort, group)
}

/// 选择写回目标模板：优先资源管理器已写过 bag（Mode/Sort）的那个，保持两边一致
fn pick_target_template(pidl: *const ITEMIDLIST, path: &str) -> &'static str {
    let candidates = candidate_templates(path);
    candidates
        .iter()
        .find(|tpl| {
            open_bag(pidl, tpl, SHGVSPB_FOLDERNODEFAULTS)
                .map(|bag| read_u32(&bag, "Mode").is_some() || read_binary(&bag, "Sort").is_some())
                .unwrap_or(false)
        })
        .copied()
        .unwrap_or(candidates[0])
}

/// 该文件夹当前生效的排序列与分组：优先 ShellBag，其次模板默认值
pub fn effective_sort_group(path: &str) -> (Vec<SortColumn>, Option<GroupCols>) {
    let Ok(pidl) = parse_to_pidl(path) else {
        return template_default_sort(path);
    };
    let mut found_sort: Option<Vec<SortColumn>> = None;
    let mut found_group: Option<Option<GroupCols>> = None;
    for tpl in candidate_templates(path) {
        let s = read_sort(pidl, tpl);
        let g = read_group(pidl, tpl);
        if s.is_some() || g.is_some() {
            found_sort = s;
            found_group = g;
            break;
        }
    }
    unsafe { ILFree(Some(pidl)) };
    if found_sort.is_none() && found_group.is_none() {
        return template_default_sort(path);
    }
    let (dsort, dgroup) = template_default_sort(path);
    (
        found_sort.unwrap_or(dsort),
        // bag 未记录分组 -> 模板默认；记录了（含显式不分组）-> 按记录
        match found_group {
            None => dgroup,
            Some(g) => g,
        },
    )
}

/// 写回排序列（前端点击列头/排序菜单时调用），资源管理器下次打开同样生效
pub fn save_sort(path: &str, key: &str, ascending: bool) -> bool {
    let Some(pk) = frontend_key_to_pkey(key) else {
        return false;
    };
    let Ok(pidl) = parse_to_pidl(path) else {
        return false;
    };
    let col = SortColumn { fmtid: pk.fmtid, pid: pk.pid, ascending };
    let target = pick_target_template(pidl, path);
    let ok = write_sort(pidl, target, &[col]);
    unsafe { ILFree(Some(pidl)) };
    ok
}

/// 写回分组（None 表示取消分组）
pub fn save_group(path: &str, key: Option<String>) -> bool {
    let group = match &key {
        Some(k) => match frontend_key_to_pkey(k) {
            Some(pk) => Some(GroupCols {
                fmtid: pk.fmtid,
                pid: pk.pid,
                ascending: default_dir_for(&pk),
            }),
            None => return false,
        },
        None => None,
    };
    let Ok(pidl) = parse_to_pidl(path) else {
        return false;
    };
    let target = pick_target_template(pidl, path);
    let ok = write_group(pidl, target, group);
    unsafe { ILFree(Some(pidl)) };
    ok
}
