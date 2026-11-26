use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn secsetter(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_secsetter"))
        .current_dir(root)
        .env("XDG_CONFIG_HOME", root)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: Output) -> String {
    assert!(
        output.status.success() && output.stderr.is_empty(),
        "{output:?}"
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn help_is_available_without_a_config() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();

    let global = stdout(secsetter(root, &["--help"]));
    assert!(global.contains("secsetter -c ./draft.toml validate"));
    assert!(global.contains("$XDG_CONFIG_HOME/secsetter/config.toml"));
    assert_eq!(
        stdout(secsetter(root, &["-c", "missing.toml", "--help"])),
        global
    );

    for (name, syntax) in [
        ("validate", "validate"),
        ("list", "list"),
        ("install", "install <id>"),
        ("set", "set <id>"),
        ("clear", "clear <id>"),
        ("restore", "restore <id> [--replace]"),
        ("login", "login <id>"),
        ("skill", "skill"),
    ] {
        assert!(global.contains(syntax));
        let help = stdout(secsetter(root, &["-c", "missing.toml", name, "--help"]));
        assert!(help.starts_with(&format!("usage: secsetter [-c <file>] {syntax}\n")));
    }

    let unknown = secsetter(root, &["unknown", "--help"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(unknown.stdout.is_empty());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown command"));
}

#[test]
fn skill_prints_a_working_config_example_without_loading_a_config() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let guide = stdout(secsetter(root, &["skill"]));
    assert!(guide.contains("secsetter -c ./draft.toml validate"));
    assert_eq!(
        stdout(secsetter(root, &["-c", "missing.toml", "skill"])),
        guide
    );
    let example = guide
        .split_once("```toml\n")
        .and_then(|(_, text)| text.split_once("\n```"))
        .unwrap()
        .0;
    fs::write(root.join("draft.toml"), example).unwrap();
    assert_eq!(
        stdout(secsetter(root, &["-c", "draft.toml", "validate"])),
        "Config is valid.\n"
    );
}
