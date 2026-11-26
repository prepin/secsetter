mod app;
mod config;
mod fnox;
mod login;
mod paths;
mod tui;

use std::io::{self, IsTerminal};
use std::process::ExitCode;
use std::{env, ffi::OsStr};

use app::{Action, App, Status};

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
                let id = entry.id.escape_debug();
                let label = entry.label.escape_debug();
                match app.status(entry) {
                    Status::Untracked => println!("{id}\t{label}"),
                    status => println!("{id}\t{label}\t{status}"),
                }
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
        [command, id] if command == "login" => {
            let id = id.to_str().ok_or("entry ID must be UTF-8")?;
            let app = App::load()?;
            return Err(format!(
                "login {id:?}: {}",
                login::exec(app.login_command(id)?)
            ));
        }
        [command, id] if command == "restore" => {
            restore(id, false)?;
        }
        [command, id, flag] if command == "restore" && flag == "--replace" => {
            restore(id, true)?;
        }
        _ => {
            return Err(
                "usage: secsetter [list | install <id> | set <id> | restore <id> [--replace] | login <id>]"
                    .into(),
            );
        }
    }
    Ok(())
}

fn restore(id: &OsStr, replace: bool) -> Result<(), String> {
    let id = id.to_str().ok_or("entry ID must be UTF-8")?;
    println!("{}", App::load()?.perform(id, Action::Restore { replace })?);
    Ok(())
}
