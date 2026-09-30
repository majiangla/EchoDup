mod align;
mod app;
pub mod asr;
pub mod clipboard;
mod drop;
mod fonts;
pub mod player;

use std::path::PathBuf;
use std::process::ExitCode;

pub fn run_empty() -> ExitCode {
    app::run(None);
    ExitCode::SUCCESS
}

pub fn run_with_file(path: PathBuf) -> ExitCode {
    app::run(Some(path));
    ExitCode::SUCCESS
}
