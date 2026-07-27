// 通过 IShellItemImageFactory 提取与资源管理器一致的文件/文件夹图标，输出 PNG data URL
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

use base64::Engine;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::Foundation::SIZE;
use windows::Win32::UI::Shell::{IShellItemImageFactory, SIIGBF, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY, SIIGBF_RESIZETOFIT};

use crate::shell_items::item_from_path;

fn icon_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/* ===== 磁盘持久缓存（类似资源管理器 thumbcache，重启后免重新提取） ===== */

fn cache_dir() -> Option<&'static std::path::PathBuf> {
    static DIR: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let base = std::env::var_os("LOCALAPPDATA")?;
        let dir = std::path::PathBuf::from(base).join("FileSpace").join("iconcache");
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    })
    .as_ref()
}

fn hash_key(s: &str) -> u64 {
    // FNV-1a
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn disk_load(key: &str) -> Option<String> {
    let dir = cache_dir()?;
    let bytes = std::fs::read(dir.join(format!("{:016x}.png", hash_key(key)))).ok()?;
    Some(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

fn disk_store(key: &str, data_url: &str) {
    let Some(dir) = cache_dir() else { return };
    let Some(b64) = data_url.strip_prefix("data:image/png;base64,") else { return };
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
        let _ = std::fs::write(dir.join(format!("{:016x}.png", hash_key(key))), bytes);
    }
}

/// 文件修改时间标记（缩略图磁盘缓存失效用）；虚拟路径返回空
fn mtime_tag(path: &str) -> String {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

/// 提取图标/缩略图为 BGRA 像素，编码 PNG，返回 data URL。必须在 STA 线程调用。
fn extract_image_png(path: &str, size: i32, flags: SIIGBF) -> Option<String> {
    unsafe {
        let item = item_from_path(path).ok()?;
        let factory: IShellItemImageFactory = windows::core::Interface::cast(&item).ok()?;
        let hbmp = factory
            .GetImage(SIZE { cx: size, cy: size }, flags)
            .ok()?;
        let result = hbitmap_to_data_url(hbmp);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        result
    }
}

/// 将 32bpp HBITMAP 转为 PNG data URL（不释放位图，由调用方负责）
pub fn hbitmap_to_data_url(hbmp: windows::Win32::Graphics::Gdi::HBITMAP) -> Option<String> {
    unsafe {
        // 按真实位图尺寸读取（缩略图/菜单位图尺寸不定）
        let mut bm = BITMAP::default();
        if GetObjectW(HGDIOBJ(hbmp.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as _)) == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        if w <= 0 || h <= 0 {
            return None;
        }

        let hdc = GetDC(None);
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let got = GetDIBits(
            hdc,
            hbmp,
            0,
            h as u32,
            Some(pixels.as_mut_ptr() as _),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);
        if got == 0 {
            return None;
        }

        // BGRA -> RGBA；若整张图 alpha 全 0，视为不带 alpha 的图标
        let all_alpha_zero = pixels.chunks_exact(4).all(|p| p[3] == 0);
        for p in pixels.chunks_exact_mut(4) {
            p.swap(0, 2);
            if all_alpha_zero {
                p[3] = 255;
            }
        }

        let mut png_data = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_data, w as u32, h as u32);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().ok()?;
            writer.write_image_data(&pixels).ok()?;
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png_data);
        Some(format!("data:image/png;base64,{b64}"))
    }
}

/// 批量获取图标（内存 + 磁盘两级缓存）。key 规则：
/// - 普通文件按扩展名缓存；exe/lnk/ico/url/cur 及文件夹按完整路径缓存
pub fn get_icon_cached(path: &str, is_folder: bool, ext: &str, size: i32) -> Option<String> {
    let per_path = is_folder || matches!(ext, "exe" | "lnk" | "ico" | "url" | "cur" | "" );
    let key = if per_path {
        format!("p:{size}:{path}")
    } else {
        format!("e:{size}:{ext}")
    };
    if let Some(hit) = icon_cache().lock().unwrap().get(&key) {
        return Some(hit.clone());
    }
    if let Some(hit) = disk_load(&key) {
        icon_cache().lock().unwrap().insert(key, hit.clone());
        return Some(hit);
    }
    let icon = extract_image_png(path, size, SIIGBF_ICONONLY)?;
    disk_store(&key, &icon);
    icon_cache().lock().unwrap().insert(key, icon.clone());
    Some(icon)
}

/// 缩略图（图片/视频显真实缩略图，其余回退到图标），用于中/大/超大图标视图与预览窗格。
/// 磁盘缓存 key 含文件修改时间，文件变化后自动失效；虚拟路径仅内存缓存
pub fn get_thumbnail_cached(path: &str, size: i32) -> Option<String> {
    let key = format!("t:{size}:{path}");
    if let Some(hit) = icon_cache().lock().unwrap().get(&key) {
        return Some(hit.clone());
    }
    let mtime = mtime_tag(path);
    let disk_key = if mtime.is_empty() { String::new() } else { format!("{key}:{mtime}") };
    if !disk_key.is_empty() {
        if let Some(hit) = disk_load(&disk_key) {
            icon_cache().lock().unwrap().insert(key, hit.clone());
            return Some(hit);
        }
    }
    let img = extract_image_png(path, size, SIIGBF_RESIZETOFIT | SIIGBF_BIGGERSIZEOK)?;
    if !disk_key.is_empty() {
        disk_store(&disk_key, &img);
    }
    icon_cache().lock().unwrap().insert(key, img.clone());
    Some(img)
}
