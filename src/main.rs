mod app;
mod config;
mod paths;
mod tui;

use std::io::{self, IsTerminal};
use std::process::ExitCode;
use std::{env, ffi::OsStr};

use app::{Action, App};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("secsetter: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => tui::run(&App::load()?).map_err(|error| format!("terminal: {error}"))?,
        [command] if command == "list" => {
            let app = App::load()?;
            for entry in app.entries() {
                println!(
                    "{}\t{}\t{}",
                    entry.id.escape_debug(),
                    entry.label.escape_debug(),
                    app.status(entry)
                );
            }
        }
        [command, id] if command == "install" => {
            let id = id.to_str().ok_or("entry ID must be UTF-8")?;
            println!("{}", App::load()?.perform(id, Action::Install)?);
        }
        [command, id] if command == "set" => {
            let id = id.to_str().ok_or("entry ID must be UTF-8")?;
            let stdin = io::stdin();
            if stdin.is_terminal() {
                return Err("set requires non-terminal standard input".into());
            }
            println!(
                "{}",
                App::load()?.perform(id, Action::Set(&mut stdin.lock()))?
            );
        }
        [command, id] if command == "restore" || command == "login" => {
            return reject_mismatch(command, id);
        }
        [command, id, flag] if command == "restore" && flag == "--replace" => {
            return reject_mismatch(command, id);
        }
        _ => return Err("usage: secsetter [list | install <id> | set <id>]".into()),
    }
    Ok(())
}

fn reject_mismatch(command: &OsStr, id: &OsStr) -> Result<(), String> {
    let id = id.to_str().ok_or("entry ID must be UTF-8")?;
    App::load()?.entry(id)?;
    Err(format!("{command:?} does not match entry {id:?}"))
}
