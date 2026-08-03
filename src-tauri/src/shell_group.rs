// 分组引擎：优先用 Shell 自带的分类器（ICategoryProvider/ICategorizer）取得与资源管理器
// 完全一致的本地化分组名；取不到时（虚拟命名空间等）回退按日期/大小/类型/首字母分桶。
// 组的排列顺序统一按"分组列属性值"排序后的首次出现顺序确定（与资源管理器一致）。
use std::collections::HashMap;

use windows::core::Interface;
use windows::Win32::Foundation::{FILETIME, HWND};
use windows::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
use windows::Win32::System::Com::StructuredStorage::{
    PropVariantClear, PropVariantCompareEx, PROPVARIANT, PVCF_DEFAULT, PVCU_DEFAULT,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    ICategoryProvider, ICategorizer, IParentAndItem, IShellFolder, IShellItem2, ILFree,
    BHID_SFObject, CATEGORY_INFO, SIGDN_NORMALDISPLAY,
};

use crate::shell_items::{column_to_frontend_key, pwstr_to_string_free, GroupCols};

/// 计算每项的 (分组序号, 分组名)，与 items 顺序对齐。返回 None 表示无法分组。
pub fn compute(
    folder: &IShellItem2,
    items: &[IShellItem2],
    col: GroupCols,
) -> Option<Vec<(i32, String)>> {
    let n = items.len();
    if n == 0 {
        return Some(Vec::new());
    }
    let pkey = col.pkey();
    let key = column_to_frontend_key(&col.fmtid, col.pid);

    // 每项的分组名：优先分类器，失败再回退
    let names = categorizer_names(folder, items, &pkey)
        .or_else(|| fallback_names(items, &pkey, key))?;

    // 组排序：按分组列属性值排序后的首次出现顺序
    let mut gprops: Vec<PROPVARIANT> = Vec::with_capacity(n);
    for it in items {
        gprops.push(unsafe { it.GetProperty(&pkey) }.unwrap_or_default());
    }
    let mut idx: Vec<usize> = (0..n).collect();
    let dir = if col.ascending { 1 } else { -1 };
    idx.sort_by(|&a, &b| {
        let c = unsafe { PropVariantCompareEx(&gprops[a], &gprops[b], PVCU_DEFAULT, PVCF_DEFAULT) };
        (c * dir).cmp(&0)
    });
    for pv in gprops.iter_mut() {
        unsafe {
            let _ = PropVariantClear(pv);
        }
    }

    let mut rank_of: HashMap<String, i32> = HashMap::new();
    let mut next = 0i32;
    for &i in &idx {
        if !rank_of.contains_key(&names[i]) {
            rank_of.insert(names[i].clone(), next);
            next += 1;
        }
    }

    Some(
        (0..n)
            .map(|i| (rank_of[&names[i]], names[i].clone()))
            .collect(),
    )
}

/// 用 Shell 分类器取每项本地化分组名（与资源管理器完全一致）
fn categorizer_names(
    folder: &IShellItem2,
    items: &[IShellItem2],
    pkey: &windows::Win32::Foundation::PROPERTYKEY,
) -> Option<Vec<String>> {
    unsafe {
        let psf: IShellFolder = folder.BindToHandler(None, &BHID_SFObject).ok()?;
        let provider: ICategoryProvider = psf.CreateViewObject(HWND(std::ptr::null_mut())).ok()?;
        let catid = provider.GetCategoryForSCID(pkey).ok()?;
        let categorizer: ICategorizer = provider.CreateCategory(&catid).ok()?;

        let mut name_cache: HashMap<u32, String> = HashMap::new();
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            // 取子项相对 PIDL
            let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
            let pai: IParentAndItem = match it.cast() {
                Ok(v) => v,
                Err(_) => return None,
            };
            if pai
                .GetParentAndItem(None, None, Some(&mut child))
                .is_err()
                || child.is_null()
            {
                out.push(String::new());
                continue;
            }
            let arr = [child as *const ITEMIDLIST];
            let mut cat = 0u32;
            let got = categorizer.GetCategory(1, arr.as_ptr(), &mut cat).is_ok();
            ILFree(Some(child));
            if !got {
                out.push(String::new());
                continue;
            }
            let name = name_cache.entry(cat).or_insert_with(|| {
                let mut info = CATEGORY_INFO::default();
                if categorizer.GetCategoryInfo(cat, &mut info).is_ok() {
                    let end = info
                        .wszName
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(info.wszName.len());
                    String::from_utf16_lossy(&info.wszName[..end])
                } else {
                    String::new()
                }
            });
            out.push(name.clone());
        }
        Some(out)
    }
}

/// 回退分桶：按日期/大小/类型/首字母生成分组名
fn fallback_names(
    items: &[IShellItem2],
    pkey: &windows::Win32::Foundation::PROPERTYKEY,
    key: Option<&str>,
) -> Option<Vec<String>> {
    let key = key?;
    let now_day = now_local_day();
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        let label = match key {
            "date" | "created" => match unsafe { it.GetFileTime(pkey) } {
                Ok(ft) => local_day_index(&ft)
                    .map(|d| date_bucket(now_day - d).to_string())
                    .unwrap_or_else(|| "未指定".to_string()),
                Err(_) => "未指定".to_string(),
            },
            "size" => match unsafe { it.GetUInt64(pkey) } {
                Ok(b) => size_bucket(b).to_string(),
                Err(_) => "未指定".to_string(),
            },
            "type" => unsafe { it.GetString(pkey) }
                .ok()
                .map(pwstr_to_string_free)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "未指定".to_string()),
            "name" => unsafe { it.GetDisplayName(SIGDN_NORMALDISPLAY) }
                .ok()
                .map(pwstr_to_string_free)
                .and_then(|s| s.chars().next())
                .map(name_bucket)
                .unwrap_or_else(|| "其他".to_string()),
            _ => return None,
        };
        out.push(label);
    }
    Some(out)
}

fn date_bucket(diff_days: i64) -> &'static str {
    match diff_days {
        d if d < 0 => "以后",
        0 => "今天",
        1 => "昨天",
        2..=6 => "本周早些时候",
        7..=13 => "上周",
        14..=30 => "本月早些时候",
        31..=365 => "今年早些时候",
        _ => "很久以前",
    }
}

fn size_bucket(bytes: u64) -> &'static str {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    match bytes {
        0 => "0 字节",
        b if b < 10 * KB => "微小 (0 - 10 KB)",
        b if b < 100 * KB => "小 (10 - 100 KB)",
        b if b < MB => "中 (100 KB - 1 MB)",
        b if b < 16 * MB => "大 (1 - 16 MB)",
        b if b < 128 * MB => "很大 (16 - 128 MB)",
        _ => "巨大 (> 128 MB)",
    }
}

fn name_bucket(c: char) -> String {
    if c.is_ascii_alphabetic() {
        c.to_ascii_uppercase().to_string()
    } else if c.is_ascii_digit() {
        "0 - 9".to_string()
    } else {
        "其他".to_string()
    }
}

fn local_day_index(ft: &FILETIME) -> Option<i64> {
    let mut lft = FILETIME::default();
    unsafe { FileTimeToLocalFileTime(ft, &mut lft).ok()? };
    let t = ((lft.dwHighDateTime as u64) << 32) | lft.dwLowDateTime as u64;
    Some((t / 864_000_000_000) as i64)
}

fn now_local_day() -> i64 {
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let ticks = ((unix_ms + 11_644_473_600_000) as u64) * 10_000;
    let ft = FILETIME {
        dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    local_day_index(&ft).unwrap_or(0)
}
