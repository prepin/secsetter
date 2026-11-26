mod app;
mod config;
mod file_write;
mod fnox;
mod help;
mod login;
mod paths;
mod privileged;
mod secret_field;
mod tui;

use std::io::{self, IsTerminal};
use std::path::Path;
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
    let (config_path, args) = match args.as_slice() {
        [flag, path, args @ ..] if flag == "-c" => (Some(Path::new(path)), args),
        [flag] if flag == "-c" => return Err("-c requires a config file path".into()),
        args => (None, args),
    };
    match args {
        [command, args @ ..] if config_path.is_none() && command == privileged::HELPER => {
            privileged::run_helper(args)?
        }
        [flag] if flag == "--help" => help::print(),
        [command, flag] if flag == "--help" => help::print_command(command)?,
        [command] if command == "skill" => print!("{}", help::SKILL),
        [] => tui::run(&App::load(config_path)?).map_err(|error| format!("terminal: {error}"))?,
        [command] if command == "validate" => {
            config::Config::load(config_path)?;
            println!("Config is valid.");
        }
        [command] if command == "list" => {
            let app = App::load(config_path)?;
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
            println!("{}", App::load(config_path)?.perform(id, Action::Install)?);
        }
        [command, id] if command == "set" => {
            let id = id.to_str().ok_or("entry ID must be UTF-8")?;
            let stdin = io::stdin();
            if stdin.is_terminal() {
                return Err("set requires non-terminal standard input".into());
            }
            println!(
                "{}",
                App::load(config_path)?.perform(id, Action::Set(&mut stdin.lock()))?
            );
        }
        [command, id] if command == "clear" => {
            let id = id.to_str().ok_or("entry ID must be UTF-8")?;
            println!("{}", App::load(config_path)?.perform(id, Action::Clear)?);
        }
        [command, id] if command == "login" => {
            let id = id.to_str().ok_or("entry ID must be UTF-8")?;
            let app = App::load(config_path)?;
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
            restore(config_path, id, false)?;
        }
        [command, id, flag] if command == "restore" && flag == "--replace" => {
            restore(config_path, id, true)?;
        }
        _ => {
            return Err(format!(
                "{}\nRun 'secsetter --help' to see the commands.",
                help::USAGE
            ));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn restore(config_path: Option<&Path>, id: &OsStr, replace: bool) -> Result<(), String> {
    let id = id.to_str().ok_or("entry ID must be UTF-8")?;
    println!(
        "{}",
        App::load(config_path)?.perform(id, Action::Restore { replace })?
    );
    Ok(())
}
