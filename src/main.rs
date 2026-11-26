mod app;
mod config;
mod file_write;
mod fnox;
mod login;
mod paths;
mod privileged;
mod secret_field;
mod tui;

use std::io::{self, IsTerminal};
use std::process::ExitCode;
use std::{env, ffi::OsStr};

use app::{Action, App, Status};

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("secsetter: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    match args.as_slice() {
        [command, args @ ..] if command == privileged::HELPER => privileged::run_helper(args)?,
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
        [command, id] if command == "clear" => {
            let id = id.to_str().ok_or("entry ID must be UTF-8")?;
            println!("{}", App::load()?.perform(id, Action::Clear)?);
        }
        [command, id] if command == "login" => {
            let id = id.to_str().ok_or("entry ID must be UTF-8")?;
            let app = App::load()?;
            let commands = app.login_commands(id)?;
            let result = {
                let interrupt = login::InterruptGuard::new()
                    .map_err(|error| format!("login {id:?}: {error}"))?;
                login::run(commands, &interrupt)
            };
            if let Err(failure) = result {
                eprintln!("secsetter: login {id:?}: {failure}");
                return Ok(failure.exit_code());
            }
        }
        [command, id] if command == "restore" => {
            restore(id, false)?;
        }
        [command, id, flag] if command == "restore" && flag == "--replace" => {
            restore(id, true)?;
        }
        _ => {
            return Err(
                "usage: secsetter [list | install <id> | set <id> | clear <id> | restore <id> [--replace] | login <id>]"
                    .into(),
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn restore(id: &OsStr, replace: bool) -> Result<(), String> {
    let id = id.to_str().ok_or("entry ID must be UTF-8")?;
    println!("{}", App::load()?.perform(id, Action::Restore { replace })?);
    Ok(())
}
