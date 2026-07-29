// Shell 命名空间枚举与属性读取 —— 与资源管理器完全一致的数据来源
use serde::Serialize;
use windows::core::{Interface, GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{FILETIME, PROPERTYKEY};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
use windows::Win32::System::SystemServices::{
    SFGAO_FILESYSTEM, SFGAO_FOLDER, SFGAO_HIDDEN, SFGAO_STREAM,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IEnumShellItems, IShellItem, IShellItem2, SHCreateItemFromParsingName, SHFormatDateTimeW,
    StrFormatKBSizeW, BHID_EnumItems, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_FILESYSPATH,
    SIGDN_NORMALDISPLAY,
};

// This PC / 快速访问 / 网络 / Linux(WSL) 的解析名
pub const THIS_PC: &str = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
pub const QUICK_ACCESS: &str = "shell:::{679f85cb-0220-4080-b29b-5540cc05aab6}";
pub const NETWORK: &str = "::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}";
pub const LINUX_WSL: &str = "::{B2B4A4D1-2754-4140-A2EB-9A76D9D7CDC6}";

// 手动定义属性键（Storage FMTID）
const FMTID_STORAGE: GUID = GUID::from_u128(0xB725F130_47EF_101A_A5F1_02608C9EEBAC);
const PKEY_ITEM_TYPE_TEXT: PROPERTYKEY = PROPERTYKEY { fmtid: FMTID_STORAGE, pid: 4 };
const PKEY_SIZE: PROPERTYKEY = PROPERTYKEY { fmtid: FMTID_STORAGE, pid: 12 };
const PKEY_DATE_MODIFIED: PROPERTYKEY = PROPERTYKEY { fmtid: FMTID_STORAGE, pid: 14 };
const PKEY_DATE_CREATED: PROPERTYKEY = PROPERTYKEY { fmtid: FMTID_STORAGE, pid: 15 };
// System.Image.HorizontalSize / VerticalSize
const FMTID_IMAGE: GUID = GUID::from_u128(0x6444048F_4C8B_11D1_8B70_080036B11A03);
const PKEY_IMAGE_H: PROPERTYKEY = PROPERTYKEY { fmtid: FMTID_IMAGE, pid: 3 };
const PKEY_IMAGE_V: PROPERTYKEY = PROPERTYKEY { fmtid: FMTID_IMAGE, pid: 4 };

#[derive(Serialize, Clone, Default)]
pub struct ShellEntry {
    pub name: String,
    pub full_name: String,
    pub parse_path: String,
    pub fs_path: Option<String>,
    pub is_folder: bool,
    pub sort_as_folder: bool,
    pub is_hidden: bool,
    pub size: Option<u64>,
    pub size_text: String,
    pub date_modified: i64,
    pub date_text: String,
    pub date_created: i64,
    pub date_created_text: String,
    pub type_text: String,
    pub ext: String,
    /// 仅驱动器：总容量/可用容量与格式化文本（"38.8 GB 可用, 共 200 GB"）
    pub drive_total: Option<u64>,
    pub drive_free: Option<u64>,
    pub drive_text: String,
    /// 仅快速访问：用户置顶（true）或最近访问的常用文件夹（false）
    pub pinned: bool,
}

#[derive(Serialize)]
pub struct Crumb {
    pub name: String,
    pub parse_path: String,
}

#[derive(Serialize)]
pub struct FolderListing {
    pub folder_name: String,
    pub parse_path: String,
    pub breadcrumb: Vec<Crumb>,
    pub entries: Vec<ShellEntry>,
}

#[derive(Serialize)]
pub struct SidebarData {
    pub quick_access: Vec<ShellEntry>,
    pub this_pc: Vec<ShellEntry>,
    pub drives: Vec<ShellEntry>,
    /// 网络节点（总是存在）与 Linux/WSL 节点（仅安装了 WSL 时）
    pub network: Option<ShellEntry>,
    pub linux: Option<ShellEntry>,
}

pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(crate) fn pwstr_to_string_free(p: PWSTR) -> String {
    unsafe {
        let s = if p.is_null() { String::new() } else { p.to_string().unwrap_or_default() };
        CoTaskMemFree(Some(p.as_ptr() as _));
        s
    }
}

pub fn item_from_path(path: &str) -> windows::core::Result<IShellItem2> {
    let wide = to_wide(path);
    unsafe { SHCreateItemFromParsingName::<PCWSTR, Option<&IBindCtx>, IShellItem2>(PCWSTR(wide.as_ptr()), None) }
}

fn display_name(item: &IShellItem2, sigdn: windows::Win32::UI::Shell::SIGDN) -> Option<String> {
    unsafe {
        item.GetDisplayName(sigdn)
            .ok()
            .map(pwstr_to_string_free)
            .filter(|s| !s.is_empty())
    }
}

/// 用与资源管理器相同的格式（本地化短日期+短时间）格式化 FILETIME
fn format_date(ft: &FILETIME) -> String {
    unsafe {
        let mut buf = [0u16; 128];
        let len = SHFormatDateTimeW(ft, None, &mut buf);
        if len > 0 {
            String::from_utf16_lossy(&buf[..(len as usize - 1).min(buf.len())])
        } else {
            String::new()
        }
    }
}

/// 与资源管理器一致的大小格式（"1,497 KB"）
fn format_size_kb(bytes: u64) -> String {
    unsafe {
        let mut buf = [0u16; 64];
        let r = StrFormatKBSizeW(bytes as i64, &mut buf);
        if r.is_null() {
            return format!("{} KB", bytes.div_ceil(1024));
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }
}

fn filetime_to_unix_ms(ft: &FILETIME) -> i64 {
    let t = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    // FILETIME: 100ns since 1601-01-01
    ((t / 10_000) as i64) - 11_644_473_600_000
}

/// 资源管理器风格字节数（"38.8 GB"）
fn format_byte_size(bytes: u64) -> String {
    use windows::Win32::UI::Shell::StrFormatByteSizeW;
    unsafe {
        let mut buf = [0u16; 64];
        let r = StrFormatByteSizeW(bytes as i64, &mut buf);
        if r.is_null() {
            return format!("{} B", bytes);
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }
}

/// 驱动器根目录取容量信息
fn drive_info(root: &str) -> Option<(u64, u64, String)> {
    use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW};
    let wide = to_wide(root);
    unsafe {
        // 网络驱动器（DRIVE_REMOTE=4）跳过：断开的映射盘会阻塞数秒拖慢此电脑枚举
        if GetDriveTypeW(PCWSTR(wide.as_ptr())) == 4 {
            return None;
        }
    }
    let mut free = 0u64;
    let mut total = 0u64;
    unsafe {
        GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free), Some(&mut total), None).ok()?;
    }
    if total == 0 {
        return None;
    }
    let text = format!("{} 可用, 共 {}", format_byte_size(free), format_byte_size(total));
    Some((total, free, text))
}

pub fn entry_from_item(item: &IShellItem2) -> Option<ShellEntry> {
    unsafe {
        let name = display_name(item, SIGDN_NORMALDISPLAY)?;
        let parse_path = display_name(item, SIGDN_DESKTOPABSOLUTEPARSING)?;
        let fs_path = display_name(item, SIGDN_FILESYSPATH);

        let attrs = item
            .GetAttributes(SFGAO_FOLDER | SFGAO_FILESYSTEM | SFGAO_HIDDEN | SFGAO_STREAM)
            .unwrap_or_default();
        let is_folder = (attrs.0 & SFGAO_FOLDER.0) != 0;
        let is_stream = (attrs.0 & SFGAO_STREAM.0) != 0;
        let is_hidden = (attrs.0 & SFGAO_HIDDEN.0) != 0;
        // zip 等"文件夹+流"按文件排序（与资源管理器一致）
        let sort_as_folder = is_folder && !is_stream;

        let size = item.GetUInt64(&PKEY_SIZE).ok();
        let size_text = match size {
            Some(s) if !sort_as_folder => format_size_kb(s),
            _ => String::new(),
        };

        let (date_modified, date_text) = match item.GetFileTime(&PKEY_DATE_MODIFIED) {
            Ok(ft) => (filetime_to_unix_ms(&ft), format_date(&ft)),
            Err(_) => (0, String::new()),
        };
        let (date_created, date_created_text) = match item.GetFileTime(&PKEY_DATE_CREATED) {
            Ok(ft) => (filetime_to_unix_ms(&ft), format_date(&ft)),
            Err(_) => (0, String::new()),
        };

        let type_text = item
            .GetString(&PKEY_ITEM_TYPE_TEXT)
            .ok()
            .map(pwstr_to_string_free)
            .unwrap_or_default();

        let ext = fs_path
            .as_deref()
            .and_then(|p| p.rsplit('.').next())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();

        // 完整文件名（含扩展名，供"文件扩展名"开关使用）
        let full_name = fs_path
            .as_deref()
            .and_then(|p| p.rsplit('\\').next())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| name.clone());

        // 驱动器容量（parse_path 形如 "X:\\"）
        let pb = parse_path.as_bytes();
        let is_drive_root = pb.len() == 3 && pb[1] == b':' && pb[2] == b'\\';
        let (drive_total, drive_free, drive_text) = if is_drive_root {
            match drive_info(&parse_path) {
                Some((t, f, s)) => (Some(t), Some(f), s),
                None => (None, None, String::new()),
            }
        } else {
            (None, None, String::new())
        };

        Some(ShellEntry {
            name,
            full_name,
            parse_path,
            fs_path,
            is_folder: sort_as_folder,
            sort_as_folder,
            is_hidden,
            size,
            size_text,
            date_modified,
            date_text,
            date_created,
            date_created_text,
            type_text,
            ext,
            drive_total,
            drive_free,
            drive_text,
            pinned: false,
        })
    }
}

pub fn enumerate_folder(path: &str) -> Result<Vec<ShellEntry>, String> {
    let item = item_from_path(path).map_err(|e| e.message())?;
    enumerate_item(&item)
}

fn enumerate_item(item: &IShellItem2) -> Result<Vec<ShellEntry>, String> {
    let mut out = Vec::new();
    unsafe {
        let enumerator: IEnumShellItems = item
            .BindToHandler(None, &BHID_EnumItems)
            .map_err(|e| e.message())?;
        loop {
            let mut fetched = 0u32;
            let mut items: [Option<IShellItem>; 64] = std::array::from_fn(|_| None);
            let _ = enumerator.Next(&mut items, Some(&mut fetched));
            if fetched == 0 {
                break;
            }
            for it in items.iter().take(fetched as usize).flatten() {
                let item2: IShellItem2 = match it.cast() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                // 隐藏项也返回，由前端"隐藏的项目"开关决定是否显示
                if let Some(entry) = entry_from_item(&item2) {
                    out.push(entry);
                }
            }
        }
    }
    Ok(out)
}

/// 桌面命名空间根（空 PIDL）的子项：主文件夹/OneDrive/此电脑/网络/桌面文件夹内容等，
/// 供地址栏最前的根下拉使用（与资源管理器一致）
pub fn desktop_entries() -> Vec<ShellEntry> {
    use windows::Win32::UI::Shell::SHCreateItemFromIDList;
    unsafe {
        // 空 PIDL（仅含终止符）表示桌面根
        let empty: [u8; 2] = [0, 0];
        let item: IShellItem2 = match SHCreateItemFromIDList(empty.as_ptr() as *const ITEMIDLIST) {
            Ok(i) => i,
            Err(_) => return Vec::new(),
        };
        enumerate_item(&item).unwrap_or_default()
    }
}

/// 通过 GetParent 链构建面包屑（桌面根节点被剔除，与资源管理器一致）。
/// 盘符路径统一加"此电脑"前缀解析：部分卷直接解析会得到桌面直下的扁平 PIDL，
/// 导致父级链缺少"此电脑"（如 F:/G: 盘，与 C:/D: 行为不一致）
pub fn breadcrumb_for(path: &str) -> Vec<Crumb> {
    let bytes = path.as_bytes();
    let is_drive_path = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    let item = if is_drive_path {
        item_from_path(&format!("{THIS_PC}\\{path}")).or_else(|_| item_from_path(path))
    } else {
        item_from_path(path)
    };
    let mut chain = Vec::new();
    if let Ok(item) = item {
        let mut cur: Option<IShellItem2> = Some(item);
        while let Some(it) = cur {
            let name = display_name(&it, SIGDN_NORMALDISPLAY).unwrap_or_default();
            let parse = display_name(&it, SIGDN_DESKTOPABSOLUTEPARSING).unwrap_or_default();
            chain.push(Crumb { name, parse_path: parse });
            cur = unsafe {
                it.GetParent()
                    .ok()
                    .and_then(|p: IShellItem| p.cast::<IShellItem2>().ok())
            };
        }
    }
    chain.reverse();
    // 移除桌面根节点（资源管理器地址栏不显示"桌面 >"前缀）
    if chain.len() > 1 {
        chain.remove(0);
    }
    chain
}

pub fn folder_listing(path: &str) -> Result<FolderListing, String> {
    let item = item_from_path(path).map_err(|e| e.message())?;
    let folder_name = display_name(&item, SIGDN_NORMALDISPLAY).unwrap_or_default();
    let parse_path = display_name(&item, SIGDN_DESKTOPABSOLUTEPARSING).unwrap_or_else(|| path.to_string());
    let entries = enumerate_folder(path)?;
    let breadcrumb = breadcrumb_for(path);
    Ok(FolderListing {
        folder_name,
        parse_path,
        breadcrumb,
        entries,
    })
}

/// 快速访问列表：枚举与资源管理器相同的 shell 命名空间，
/// 通过 System.Home.IsPinned 属性区分"已固定"与"最近访问的常用文件夹"，
/// 置顶项排前（各自保持系统枚举顺序），与资源管理器导航窗格完全一致
fn quick_access_entries() -> Vec<ShellEntry> {
    use windows::core::w;
    use windows::Win32::UI::Shell::PropertiesSystem::PSGetPropertyKeyFromName;

    let mut pin_key = PROPERTYKEY::default();
    let has_pin_key =
        unsafe { PSGetPropertyKeyFromName(w!("System.Home.IsPinned"), &mut pin_key).is_ok() };

    let Ok(item) = item_from_path(QUICK_ACCESS) else {
        return Vec::new();
    };
    let mut out: Vec<ShellEntry> = Vec::new();
    unsafe {
        let Ok(enumerator) = item.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) else {
            return Vec::new();
        };
        loop {
            let mut fetched = 0u32;
            let mut items: [Option<IShellItem>; 64] = std::array::from_fn(|_| None);
            let _ = enumerator.Next(&mut items, Some(&mut fetched));
            if fetched == 0 {
                break;
            }
            for it in items.iter().take(fetched as usize).flatten() {
                let Ok(item2) = it.cast::<IShellItem2>() else { continue };
                let Some(mut entry) = entry_from_item(&item2) else { continue };
                // 导航窗格只显示文件夹（Win11"主文件夹"里的最近文件被排除）
                if !entry.is_folder {
                    continue;
                }
                // 属性不可用（旧系统）时全部按置顶处理
                entry.pinned = if has_pin_key {
                    item2.GetBool(&pin_key).map(|b| b.as_bool()).unwrap_or(false)
                } else {
                    true
                };
                out.push(entry);
            }
        }
    }
    // 稳定排序：置顶在前，组内保持系统顺序（即资源管理器显示顺序）
    out.sort_by_key(|e| !e.pinned);
    out
}

/// 侧栏数据：快速访问（钉住的文件夹）+ 此电脑子项 + 驱动器
pub fn sidebar_data() -> SidebarData {
    let quick_access = quick_access_entries();

    let this_pc: Vec<ShellEntry> = enumerate_folder(THIS_PC).unwrap_or_default();
    let is_drive = |e: &ShellEntry| {
        let p = e.parse_path.as_bytes();
        p.len() == 3 && p[1] == b':' && p[2] == b'\\'
    };
    let drives = this_pc.iter().filter(|e| is_drive(e)).cloned().collect();

    let node_entry = |path: &str| item_from_path(path).ok().and_then(|i| entry_from_item(&i));
    let network = node_entry(NETWORK);
    let linux = node_entry(LINUX_WSL);

    SidebarData {
        quick_access,
        this_pc,
        drives,
        network,
        linux,
    }
}

/// 图片分辨率（非图片返回 None）
pub fn image_size(path: &str) -> Option<(u32, u32)> {
    let item = item_from_path(path).ok()?;
    unsafe {
        let w = item.GetUInt32(&PKEY_IMAGE_H).ok()?;
        let h = item.GetUInt32(&PKEY_IMAGE_V).ok()?;
        Some((w, h))
    }
}

/// 详细信息窗格属性列表：读 System.PropList.PreviewDetails，
/// 与资源管理器完全一致（按文件类型显示不同属性集，含本地化标签与格式化值）
pub fn item_details(path: &str) -> Vec<(String, String)> {
    use windows::Win32::UI::Shell::PropertiesSystem::{
        IPropertyDescription, IPropertyDescriptionList, PDFF_DEFAULT,
    };
    // System.PropList.PreviewDetails
    const PKEY_PREVIEW_DETAILS: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0xC9944A21_A406_48FE_8225_AEC7E24C211B),
        pid: 8,
    };
    let mut out = Vec::new();
    let Ok(item) = item_from_path(path) else {
        return out;
    };
    unsafe {
        let Ok(list) = item.GetPropertyDescriptionList::<IPropertyDescriptionList>(&PKEY_PREVIEW_DETAILS) else {
            return out;
        };
        let count = list.GetCount().unwrap_or(0);
        for i in 0..count {
            let Ok(pd) = list.GetAt::<IPropertyDescription>(i) else {
                continue;
            };
            let mut key = PROPERTYKEY::default();
            if pd.GetPropertyKey(&mut key).is_err() {
                continue;
            }
            let Ok(pv) = item.GetProperty(&key) else {
                continue;
            };
            let label = pd
                .GetDisplayName()
                .ok()
                .map(pwstr_to_string_free)
                .unwrap_or_default();
            if label.is_empty() {
                continue;
            }
            let value = pd
                .FormatForDisplay(&pv, PDFF_DEFAULT)
                .ok()
                .map(pwstr_to_string_free)
                .unwrap_or_default();
            if value.trim().is_empty() {
                continue;
            }
            out.push((label, value));
        }
    }
    out
}

/// 解析路径 -> 绝对 PIDL（调用方负责 ILFree/CoTaskMemFree）
pub fn parse_to_pidl(path: &str) -> windows::core::Result<*mut ITEMIDLIST> {
    use windows::Win32::UI::Shell::SHParseDisplayName;
    let wide = to_wide(path);
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    unsafe {
        SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None)?;
    }
    Ok(pidl)
}
