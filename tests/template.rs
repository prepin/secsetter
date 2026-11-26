use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct ConfigDir {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl ConfigDir {
    fn new(config: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        // macOS exposes its temporary directories through symlinked paths.
        let root = temp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("secsetter/templates")).unwrap();
        fs::write(root.join("secsetter/config.toml"), config).unwrap();
        Self { _temp: temp, root }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_secsetter"));
        command.env("XDG_CONFIG_HOME", &self.root);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
}

const WIDGET: &str = r#"
[[entries]]
id = "vpn-widget"
label = "VPN widget config"
kind = "template"
source = "widget.json"
destination = "$XDG_CONFIG_HOME/widget/config.json"
"#;

fn stdout(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn installs_a_missing_template_and_requires_replace_to_reinstall() {
    use std::os::unix::fs::PermissionsExt;

    let config = ConfigDir::new(WIDGET);
    let bytes = b"{\"literal\":\"$XDG_CONFIG_HOME\"}\n\0\xff";
    fs::write(config.root.join("secsetter/templates/widget.json"), bytes).unwrap();
    let destination = config.root.join("widget/config.json");

    let list = stdout(config.run(&["list"]));
    assert!(list.contains("vpn-widget") && list.contains("VPN widget config"));
    assert!(list.contains("Not installed"));

    stdout(config.run(&["install", "vpn-widget"]));
    assert_eq!(fs::read(&destination).unwrap(), bytes);
    assert_eq!(
        fs::metadata(destination.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(stdout(config.run(&["list"])).contains("Installed"));

    fs::write(&destination, b"local config").unwrap();
    let template = config.root.join("secsetter/templates/widget.json");
    fs::remove_file(&template).unwrap();
    fails(config.run(&["install", "vpn-widget"]), "--replace");
    fails(
        config.run(&["install", "vpn-widget", "--replace"]),
        "widget.json",
    );
    assert_eq!(fs::read(&destination).unwrap(), b"local config");

    fs::write(template, b"").unwrap();
    stdout(config.run(&["install", "vpn-widget", "--replace"]));
    assert!(fs::read(&destination).unwrap().is_empty());
    assert_eq!(
        fs::metadata(destination).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn lists_applicable_entries_in_order_and_checks_commands() {
    let other_platform = if cfg!(target_os = "linux") {
        "macos"
    } else {
        "linux"
    };
    let other = WIDGET.replace("vpn-widget", "other-platform").replace(
        "kind = \"template\"",
        &format!("kind = \"template\"\nplatforms = [\"{other_platform}\"]"),
    );
    let second = WIDGET.replace("vpn-widget", "another-widget");
    let config = ConfigDir::new(&format!("{WIDGET}{other}{second}"));

    let list = stdout(config.run(&["list"]));
    let mut lines = list.lines();
    assert!(lines.next().unwrap().starts_with("vpn-widget\t"));
    assert!(lines.next().unwrap().starts_with("another-widget\t"));
    assert!(lines.next().is_none());
    fails(config.run(&["install", "unknown"]), "unknown entry ID");
    fails(config.run(&["install", "other-platform"]), "does not apply");
    fails(config.run(&["set", "vpn-widget"]), "does not match");
    fails(
        config.run(&["restore", "vpn-widget", "--replace"]),
        "does not match",
    );
}

#[test]
fn reads_the_default_config_when_xdg_is_unset_or_empty() {
    let config = ConfigDir::new(&WIDGET.replace("$XDG_CONFIG_HOME/", "~/"));
    fs::create_dir(config.root.join(".config")).unwrap();
    fs::rename(
        config.root.join("secsetter"),
        config.root.join(".config/secsetter"),
    )
    .unwrap();
    fs::write(
        config.root.join(".config/secsetter/templates/widget.json"),
        b"widget",
    )
    .unwrap();

    let mut command = config.command();
    command
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", &config.root);
    assert!(
        stdout(command.args(["install", "vpn-widget"]).output().unwrap()).contains("Installed")
    );
    assert_eq!(
        fs::read(config.root.join("widget/config.json")).unwrap(),
        b"widget"
    );

    let mut command = config.command();
    command.env("XDG_CONFIG_HOME", "").env("HOME", &config.root);
    assert!(stdout(command.arg("list").output().unwrap()).contains("Installed"));
}

#[test]
fn validates_an_explicit_config_without_running_actions() {
    let config = ConfigDir::new(WIDGET);
    let draft = r#"
[[entries]]
id = "draft-login"
label = "Draft login"
kind = "login"
commands = [["/bin/sh", "-c", 'touch "$XDG_CONFIG_HOME/login-ran"']]
"#;
    let draft_path = config.root.join("draft.toml");
    fs::write(&draft_path, format!("{WIDGET}{draft}")).unwrap();

    assert_eq!(stdout(config.run(&["validate"])), "Config is valid.\n");
    let validated = config
        .command()
        .current_dir(&config.root)
        .args(["-c", "draft.toml", "validate"])
        .output()
        .unwrap();
    assert!(validated.stderr.is_empty());
    assert_eq!(stdout(validated), "Config is valid.\n");
    assert!(!config.root.join("login-ran").exists());
    assert!(!config.root.join("widget").exists());
    assert!(!config.root.join("templates").exists());

    let list = config
        .command()
        .args(["-c"])
        .arg(&draft_path)
        .arg("list")
        .output()
        .unwrap();
    assert!(stdout(list).contains("draft-login\t"));
    assert!(!stdout(config.run(&["list"])).contains("draft-login\t"));

    for file in ["missing.toml", "draft.toml/"] {
        let failed = config
            .command()
            .current_dir(&config.root)
            .args(["-c", file, "validate"])
            .output()
            .unwrap();
        assert_eq!(failed.status.code(), Some(1));
        assert!(failed.stdout.is_empty());
        fails(failed, file);
    }
    fails(config.run(&["-c"]), "-c requires a config file path");
}

#[test]
fn installs_templates_from_beside_the_selected_config() {
    let config = ConfigDir::new("");
    fs::create_dir_all(config.root.join("draft/templates")).unwrap();
    let draft = config.root.join("draft/custom.toml");
    fs::write(&draft, WIDGET).unwrap();
    fs::write(config.root.join("draft/templates/widget.json"), b"draft").unwrap();
    fs::write(
        config.root.join("secsetter/templates/widget.json"),
        b"default",
    )
    .unwrap();

    let output = config
        .command()
        .arg("-c")
        .arg(&draft)
        .args(["install", "vpn-widget"])
        .output()
        .unwrap();
    assert_eq!(stdout(output), "Installed\n");
    assert_eq!(
        fs::read(config.root.join("widget/config.json")).unwrap(),
        b"draft"
    );
}

#[test]
fn rejects_invalid_config_before_installing() {
    let other_platform = if cfg!(target_os = "linux") {
        "macos"
    } else {
        "linux"
    };
    let invalid = [
        format!("{WIDGET}\n[[broken\nsensitive-parser-excerpt"),
        format!("{WIDGET}{WIDGET}"),
        format!(
            "{WIDGET}{}",
            WIDGET
                .replace("vpn-widget", "invalid")
                .replace("\"template\"", "\"unknown\"")
        ),
        format!(
            "{WIDGET}{}",
            WIDGET
                .replace("vpn-widget", "invalid")
                .replace("source = \"widget.json\"", "")
        ),
        WIDGET.replace(
            "source = \"widget.json\"",
            &format!("source = \"../unconfined.json\"\nplatforms = [\"{other_platform}\"]"),
        ),
    ];
    for contents in invalid {
        let config = ConfigDir::new(&contents);
        fs::write(
            config.root.join("secsetter/templates/widget.json"),
            b"widget",
        )
        .unwrap();
        let error = fails(config.run(&["install", "vpn-widget"]), "invalid");
        assert!(!error.contains("sensitive-parser-excerpt"));
        let validation = config.run(&["validate"]);
        assert_eq!(validation.status.code(), Some(1));
        assert!(validation.stdout.is_empty());
        assert_eq!(fails(validation, "invalid"), error);
        assert!(!config.root.join("widget").exists());
    }
}

#[test]
fn lists_unsafe_destinations_as_blocked_and_leaves_them_intact() {
    use std::os::unix::fs::symlink;

    let config = ConfigDir::new(WIDGET);
    let outside = config.root.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("config.json"), b"existing").unwrap();
    fs::write(
        config.root.join("secsetter/templates/widget.json"),
        b"template",
    )
    .unwrap();

    let parent = config.root.join("widget");
    symlink(&outside, &parent).unwrap();
    assert!(stdout(config.run(&["list"])).contains("Blocked"));
    fails(config.run(&["install", "vpn-widget"]), "Blocked");
    assert!(parent.is_symlink());
    assert_eq!(fs::read(outside.join("config.json")).unwrap(), b"existing");

    fs::remove_file(&parent).unwrap();
    fs::create_dir(&parent).unwrap();
    let destination = parent.join("config.json");
    symlink(outside.join("absent"), &destination).unwrap();
    assert!(stdout(config.run(&["list"])).contains("Blocked"));
    fails(config.run(&["install", "vpn-widget"]), "Blocked");
    assert!(destination.is_symlink() && !outside.join("absent").exists());

    fs::remove_file(&destination).unwrap();
    fs::create_dir(&destination).unwrap();
    assert!(stdout(config.run(&["list"])).contains("Blocked"));
    fails(config.run(&["install", "vpn-widget"]), "Blocked");
    assert!(destination.is_dir());
}

#[test]
fn refuses_template_links_that_escape_the_template_directory() {
    use std::os::unix::fs::symlink;

    let config = ConfigDir::new(WIDGET);
    let templates = config.root.join("secsetter/templates");
    let outside = config.root.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("widget.json"), b"must not copy").unwrap();
    symlink(outside.join("widget.json"), templates.join("widget.json")).unwrap();
    fails(config.run(&["install", "vpn-widget"]), "symbolic link");
    assert!(!config.root.join("widget").exists());

    fs::remove_file(templates.join("widget.json")).unwrap();
    fs::remove_dir(&templates).unwrap();
    symlink(&outside, &templates).unwrap();
    fails(config.run(&["install", "vpn-widget"]), "symbolic link");
    assert!(!config.root.join("widget").exists());
}

#[test]
fn list_succeeds_when_a_destination_cannot_be_inspected() {
    use std::os::unix::fs::PermissionsExt;

    if rustix::process::geteuid().is_root() {
        return;
    }
    let config = ConfigDir::new(WIDGET);
    let parent = config.root.join("widget");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).unwrap();
    let list = config.run(&["list"]);
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(stdout(list).contains("Unknown"));
}

fn fails(output: Output, message: &str) -> String {
    assert!(!output.status.success(), "unexpected success: {output:?}");
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        error.contains(message),
        "{error:?} did not contain {message:?}"
    );
    error
}
