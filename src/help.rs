use std::ffi::OsStr;

pub(crate) const USAGE: &str = "usage: secsetter [-c <file>] [command]";
pub(crate) const SKILL: &str = include_str!("../skills/secsetter/SKILL.md");

pub(crate) fn print() {
    println!(
        "SecSetter installs templates, sets secrets, restores fnox files, and runs login actions.

{USAGE}

Without a command, SecSetter opens the terminal UI.
Use the arrow keys or j/k to move through the entry list.

Options:
  -c <file>  Select a config file. Put this option before the command.
             Relative paths start at the current directory.
  --help     Show this help. Use <command> --help for command-specific help.

Commands:
  validate                 Validate the config without running any actions.
  list                     List applicable entries and their status.
  install <id> [--replace]  Install a template.
  set <id>                 Write a secret from standard input.
  clear <id>               Clear a secret field or remove a local secret file.
  restore <id> [--replace]  Restore a file from fnox.
  login <id>               Run the configured login steps in the current terminal.
  skill                    Print the Markdown guide to writing a config.

Default config: $XDG_CONFIG_HOME/secsetter/config.toml, or
~/.config/secsetter/config.toml if XDG_CONFIG_HOME is unset or empty.

Config options:
  section     Group entries of any kind in the terminal UI: section = \"VPN\".
              Custom sections sort alphabetically, ignoring case.
              Entries without section go under Templates, Secrets (fields),
              Files (whole-file secrets), Logins, or Fnox, in that order.
  url         Link a secret-file or secret-field entry to its token page.
              Use an absolute HTTP(S) URL without credentials. Press o in the
              list, or press Ctrl+O or click Open token page in the popup.
  commands    Set the ordered steps of a login entry as arrays of literal arguments.
  ready_when  Set optional login checks: file-exists, field-set, and command.
              Add json.matches to a command check to match JSON output.
              list and the terminal UI run command checks from the config.

Validate a draft: secsetter -c ./draft.toml validate
For copyable TOML examples: secsetter skill
Help and skill do not require a config file."
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
            "Print tab-separated entry IDs, labels, and statuses for this platform.
Entries keep their config order regardless of section.
Login actions without readiness checks omit the status column.
Use a trusted config: list runs configured ready_when command checks.",
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
Set commands as arrays: commands = [[\"example\", \"login\"]].
Each step starts with an executable and passes its remaining arguments literally.
SecSetter does not interpret shell syntax. It stops at the first failure or interruption.
If you set ready_when checks and all pass, list shows Login configured.
A command check passes on exit 0 by default. If it sets json.matches, stdout
must match that pattern too. Use on_miss = \"login-needed\" for a valid nonmatch.
If a JSON check fails to run or return the expected output shape, it reports Unknown.
Readiness checks do not prevent login. For examples, run 'secsetter skill'.",
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
