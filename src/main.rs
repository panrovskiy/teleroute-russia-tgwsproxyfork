#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    if let Err(error) = tele_route::app::run() {
        let message = format!("TeleRoute failed to start:\n\n{error:#}");
        tele_route::platform::windows::write_startup_error(&message);
        tele_route::platform::windows::show_startup_error(&message);
        std::process::exit(1);
    }
}
