#![cfg_attr(windows, windows_subsystem = "windows")]

use std::panic::{catch_unwind, AssertUnwindSafe};

fn main() {
    let result = catch_unwind(AssertUnwindSafe(|| tele_route::app::run()));

    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let message = format!("TeleRoute failed to start:\n\n{error:#}");
            tele_route::platform::windows::write_startup_error(&message);
            tele_route::platform::windows::show_startup_error(&message);
            std::process::exit(1);
        }
        Err(panic) => {
            let message = format!("TeleRoute crashed unexpectedly:\n\n{panic:?}");
            tele_route::platform::windows::write_startup_error(&message);
            tele_route::platform::windows::show_startup_error(&message);
            std::process::exit(1);
        }
    }
}
