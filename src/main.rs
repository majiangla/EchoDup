#![windows_subsystem = "windows"]
mod cli;
mod config;
mod core;
mod gui;
mod proc;
mod worker;

use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(windows)]
fn enable_utf8_console() {
    use windows::Win32::System::Console::{SetConsoleCP, SetConsoleOutputCP};
    unsafe {
        let _ = SetConsoleOutputCP(65001);
        let _ = SetConsoleCP(65001);
    }
}

fn main() -> ExitCode {
    #[cfg(windows)]
    enable_utf8_console();

    let args: Vec<String> = std::env::args().collect();

    // 子进程模式：--worker <role> <payload.json>
    if let Some(pos) = args.iter().position(|a| a == "--worker") {
        return worker::run(&args[pos + 1..]);
    }

    if args.iter().any(|a| a == "--cli") {
        return cli::run(&args);
    }

    if let Some(path) = args.iter().skip(1).find(|a| !a.starts_with('-')) {
        let p = PathBuf::from(path);
        if p.is_file() {
            return gui::run_with_file(p);
        }
    }

    gui::run_empty()
}
