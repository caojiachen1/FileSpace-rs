// Tauri 命令层：将请求转发到 STA Shell 线程
use serde::Deserialize;

use crate::shell_icons;
use crate::shell_items::{self, FolderListing, SidebarData};
use crate::shell_menu::{self, MenuResult};
use crate::shell_thread::run_on_shell;

#[tauri::command]
pub async fn list_folder(path: String) -> Result<FolderListing, String> {
    run_on_shell(move || shell_items::folder_listing(&path))
}

#[tauri::command]
pub async fn get_sidebar() -> SidebarData {
    run_on_shell(shell_items::sidebar_data)
}

/// 桌面根子项（地址栏根下拉）
#[tauri::command]
pub async fn get_desktop_items() -> Vec<shell_items::ShellEntry> {
    run_on_shell(shell_items::desktop_entries)
}

#[derive(Deserialize)]
pub struct IconReq {
    pub path: String,
    pub is_folder: bool,
    pub ext: String,
}

#[tauri::command]
pub async fn get_icons(items: Vec<IconReq>, size: i32) -> Vec<Option<String>> {
    run_on_shell(move || {
        items
            .iter()
            .map(|r| shell_icons::get_icon_cached(&r.path, r.is_folder, &r.ext, size))
            .collect()
    })
}

/// 缩略图（图片显真实预览，其他回退图标）
#[tauri::command]
pub async fn get_thumbnails(paths: Vec<String>, size: i32) -> Vec<Option<String>> {
    run_on_shell(move || {
        paths
            .iter()
            .map(|p| shell_icons::get_thumbnail_cached(p, size))
            .collect()
    })
}

#[tauri::command]
pub async fn show_context_menu(
    selection: Vec<String>,
    background: Option<String>,
    state: Option<shell_menu::ViewState>,
) -> MenuResult {
    run_on_shell(move || shell_menu::show_menu(selection, background, state))
}

#[tauri::command]
pub async fn open_item(path: String) -> bool {
    run_on_shell(move || shell_menu::open_item_default(&path))
}

#[tauri::command]
pub async fn invoke_verb(
    selection: Vec<String>,
    background: Option<String>,
    verb: String,
) -> bool {
    run_on_shell(move || shell_menu::invoke_verb_direct(selection, background, &verb))
}

#[tauri::command]
pub async fn quick_access_verb(path: String, verb: String) -> bool {
    run_on_shell(move || shell_menu::quick_access_verb(&path, &verb))
}

#[tauri::command]
pub async fn rename_item(path: String, new_name: String) -> Result<(), String> {
    run_on_shell(move || shell_menu::rename(&path, &new_name))
}

#[tauri::command]
pub async fn create_folder(parent: String, name: String) -> Result<(), String> {
    run_on_shell(move || shell_menu::new_folder(&parent, &name))
}

#[tauri::command]
pub async fn watch_folder(path: String) {
    run_on_shell(move || crate::shell_watch::watch(&path))
}

#[tauri::command]
pub async fn system_action(action: String) -> bool {
    run_on_shell(move || shell_menu::system_action(&action))
}

/// 获取"新建"菜单（ShellNew 模板列表，含图标）
#[tauri::command]
pub async fn get_new_menu(folder: String) -> Vec<shell_menu::NewMenuEntry> {
    run_on_shell(move || shell_menu::get_new_menu(&folder))
}

#[tauri::command]
pub async fn invoke_new_item(id: u32) -> bool {
    run_on_shell(move || shell_menu::invoke_new(id))
}

/// Fluent 右键菜单：获取完整经典菜单树（selection 为空时取 background 背景菜单）
#[tauri::command]
pub async fn get_ctx_menu(
    selection: Vec<String>,
    background: Option<String>,
) -> Vec<shell_menu::CtxNode> {
    run_on_shell(move || shell_menu::get_ctx_menu(selection, background))
}

#[tauri::command]
pub async fn invoke_ctx_item(id: u32) -> MenuResult {
    run_on_shell(move || shell_menu::invoke_ctx(id))
}

#[tauri::command]
pub async fn close_ctx_menu() {
    run_on_shell(|| {
        shell_menu::close_ctx();
        crate::shell_modern::clear();
    })
}

/// 剪贴板是否有可粘贴的文件（背景菜单"粘贴"可用性）
#[tauri::command]
pub async fn clipboard_has_files() -> bool {
    run_on_shell(shell_menu::clipboard_has_files)
}

/// 格式化驱动器（系统对话框，独立线程，不阻塞 shell 线程）
#[tauri::command]
pub async fn format_drive(letter: String) {
    if let Some(c) = letter.chars().next() {
        shell_menu::format_drive(c);
    }
}

/// 在新窗口中打开：启动新实例并把目标路径作为启动参数
#[tauri::command]
pub async fn open_new_window(path: String) -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| std::process::Command::new(exe).arg(path).spawn().ok())
        .is_some()
}

/// 启动参数中的初始路径（"在新窗口中打开"新实例用）
#[tauri::command]
pub async fn get_start_path() -> Option<String> {
    let a = std::env::args().nth(1)?;
    // 仅接受路径样式参数（盘符/UNC/Shell 命名空间）
    let looks_like_path =
        a.starts_with("\\\\") || a.starts_with("::") || a.as_bytes().get(1) == Some(&b':');
    looks_like_path.then_some(a)
}

/// Win11 现代菜单扩展（Windows.FileExplorerContextMenus 契约 + IExplorerCommand）
#[tauri::command]
pub async fn get_modern_menu(selection: Vec<String>) -> Vec<crate::shell_modern::ModernNode> {
    run_on_shell(move || crate::shell_modern::get_modern_menu(selection))
}

#[tauri::command]
pub async fn invoke_modern_item(mid: u32) -> bool {
    run_on_shell(move || crate::shell_modern::invoke_modern(mid))
}

/// 查询文件夹在 ShellBag 中保存的视图模式（与资源管理器共享）
#[tauri::command]
pub async fn get_view_mode(path: String) -> Option<String> {
    run_on_shell(move || crate::shell_bags::view_mode_for(&path))
}

/// 将视图模式写回 ShellBag（资源管理器可见）
#[tauri::command]
pub async fn set_view_mode(path: String, view: String) -> bool {
    run_on_shell(move || crate::shell_bags::save_view_mode(&path, &view))
}

/// 图片分辨率（详细信息窗格用）
#[tauri::command]
pub async fn get_image_size(path: String) -> Option<(u32, u32)> {
    run_on_shell(move || shell_items::image_size(&path))
}

/// 完整属性列表（PreviewDetails，与资源管理器详细信息窗格一致）
#[tauri::command]
pub async fn get_item_details(path: String) -> Vec<(String, String)> {
    run_on_shell(move || shell_items::item_details(&path))
}

// ---- 原生拖拽：以下命令刻意不走 run_on_shell ----
// DoDragDrop 模态循环在主线程运行，拖拽期间 shell 线程需保持空闲响应其他命令；
// update_drop_target 在拖拽中高频调用，走 run_on_shell 会排队甚至死锁。

#[tauri::command]
pub async fn init_drag_drop(app: tauri::AppHandle) {
    crate::drag_drop::init(&app);
}

#[tauri::command]
pub async fn start_drag(app: tauri::AppHandle, paths: Vec<String>) {
    crate::drag_drop::start_drag(app, paths);
}

#[tauri::command]
pub async fn update_drop_target(
    kind: String,
    parse_path: String,
    fs_path: Option<String>,
    name: String,
) {
    crate::drag_drop::update_drop_target(kind, parse_path, fs_path, name);
}
