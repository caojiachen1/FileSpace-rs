// FileSpace - Windows Explorer clone powered by Shell COM APIs
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod shell_icons;
mod shell_items;
mod shell_menu;
mod shell_modern;
mod shell_thread;
mod shell_watch;

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
            commands::rename_item,
            commands::create_folder,
            commands::watch_folder,
            commands::system_action,
            commands::get_new_menu,
            commands::invoke_new_item,
            commands::get_ctx_menu,
            commands::invoke_ctx_item,
            commands::close_ctx_menu,
            commands::get_modern_menu,
            commands::invoke_modern_item,
            commands::get_image_size,
            commands::get_item_details,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
