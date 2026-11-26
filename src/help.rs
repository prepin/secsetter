use std::ffi::OsStr;

pub(crate) const USAGE: &str = "usage: secsetter [-c <file>] [command]";
pub(crate) const SKILL: &str = include_str!("../skills/secsetter/SKILL.md");

pub(crate) fn print() {
    println!(
        "SecSetter installs templates, sets secrets, and runs login actions.

{USAGE}

Without a command, SecSetter opens the terminal UI.

Options:
  -c <file>  Select a config file. Relative paths start at the current directory.
  --help     Show this help.

Commands:
  validate                 Validate the config without running any actions.
  list                     List applicable entries and their local status.
  install <id> [--replace]  Install a template.
  set <id>                 Write a secret from standard input.
  clear <id>               Clear a secret field or remove a local secret file.
  restore <id> [--replace]  Restore a file from fnox.
  login <id>               Run the configured login steps in the current terminal.
  skill                    Print the Markdown guide to writing a config.

Default config: $XDG_CONFIG_HOME/secsetter/config.toml, or
~/.config/secsetter/config.toml if XDG_CONFIG_HOME is unset or empty.

Validate a draft: secsetter -c ./draft.toml validate
For command help: secsetter <command> --help"
    );
}

pub(crate) fn print_command(command: &OsStr) -> Result<(), String> {
    let (syntax, description) = match command.to_str() {
        Some("validate") => (
            "validate",
            "Check all config entries, including entries for other platforms.
SecSetter reads only the config. It does not run actions or inspect referenced files.
Exit status: 0 if valid, 1 on error.

Example: secsetter -c ./draft.toml validate
For config examples, run 'secsetter skill'.",
        ),
        Some("list") => (
            "list",
            "Print tab-separated entry IDs, labels, and local statuses for this platform.
Login actions without readiness checks omit the status column.",
        ),
        Some("install") => (
            "install <id> [--replace]",
            "Install the template identified by <id>. The source is relative to
templates/ beside the config file.
  --replace  Allow replacement of an existing destination.",
        ),
        Some("set") => (
            "set <id>",
            "Set the manual secret identified by <id>. Supply a non-empty secret on
non-terminal standard input. An existing secret is replaced.",
        ),
        Some("clear") => (
            "clear <id>",
            "Empty the secret field or remove the local file identified by <id>.
There is no confirmation prompt. The stored fnox copy remains available.",
        ),
        Some("restore") => (
            "restore <id> [--replace]",
            "Restore the fnox file identified by <id>.
  --replace  Allow replacement of an existing destination.",
        ),
        Some("login") => (
            "login <id>",
            "Run the login action identified by <id> in the current terminal.
Steps run in order and stop on the first failure or interruption.
The command still runs if local checks show Login configured.",
        ),
        Some("skill") => (
            "skill",
            "Print the Markdown guide to writing and validating a SecSetter config.
The guide includes examples for every entry kind.",
        ),
        _ => return Err(format!("unknown command {command:?}\n{USAGE}")),
    };
    println!("usage: secsetter [-c <file>] {syntax}\n\n{description}");
    Ok(())
}
