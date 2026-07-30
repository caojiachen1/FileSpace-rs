// FileSpace - Windows Explorer clone powered by Shell COM APIs
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod drag_drop;
mod shell_bags;
mod shell_icons;
mod shell_items;
mod shell_menu;
mod shell_modern;
mod shell_thread;
mod shell_watch;
mod snap_layout;

fn main() {
    shell_thread::start();

    tauri::Builder::default()
        .setup(|app| {
            shell_watch::set_app_handle(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_folder,
            commands::get_sidebar,
            commands::get_desktop_items,
            commands::get_icons,
            commands::get_thumbnails,
            commands::show_context_menu,
            commands::open_item,
            commands::invoke_verb,
            commands::quick_access_verb,
            commands::rename_item,
            commands::create_folder,
            commands::watch_folder,
            commands::system_action,
            commands::compress_to_zip,
            commands::add_to_favorites,
            commands::set_clipboard_text,
            commands::get_new_menu,
            commands::invoke_new_item,
            commands::get_ctx_menu,
            commands::invoke_ctx_item,
            commands::close_ctx_menu,
            commands::clipboard_has_files,
            commands::format_drive,
            commands::open_new_window,
            commands::get_start_path,
            commands::get_modern_menu,
            commands::invoke_modern_item,
            commands::get_view_mode,
            commands::set_view_mode,
            commands::get_image_size,
            commands::get_item_details,
            commands::init_drag_drop,
            commands::init_snap_layout,
            commands::set_max_button_rect,
            commands::set_window_maximized,
            commands::set_caption_rects,
            commands::start_drag,
            commands::update_drop_target,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
