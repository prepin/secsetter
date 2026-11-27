use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{Mode, OFlags, open};
use rustix::io::{FdFlags, fcntl_setfd};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};

const FNOX_CONFIG: &str = r#"
[[entries]]
id = "profile"
label = "VPN profile"
kind = "fnox-file"
source = "~/.config/VPN $HOME; profile.conf"
destination = "$XDG_CONFIG_HOME/secrets/token"
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::with_config(
            r#"
[[entries]]
id = "token"
label = "Token file"
kind = "secret-file"
destination = "$XDG_CONFIG_HOME/secrets/token"
"#,
        )
    }

    fn with_config(config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir(root.join("secsetter")).unwrap();
        fs::write(root.join("secsetter/config.toml"), config).unwrap();
        Self { _dir: dir, root }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_secsetter"));
        command
            .env("XDG_CONFIG_HOME", &self.root)
            .env("PATH", &self.root);
        command
    }

    fn destination(&self) -> PathBuf {
        self.root.join("secrets/token")
    }

    fn set(&self, input: &[u8]) -> Output {
        let mut child = self
            .command()
            .args(["set", "token"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    fn list(&self) -> String {
        success(self.command().arg("list").output().unwrap())
    }

    fn clear(&self, id: &str) -> Output {
        self.command().args(["clear", id]).output().unwrap()
    }

    fn fnox_stub(&self, body: &str) {
        let executable = self.root.join("fnox");
        fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
[ "$#" = 3 ] && [ "$1" = get ] && [ "$2" = --file ] &&
  [ "$3" = '~/.config/VPN $HOME; profile.conf' ] || exit 45
: > "$XDG_CONFIG_HOME/fnox-called"
{body}
"#,
            ),
        )
        .unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn restore(&self, replace: bool) -> Output {
        let mut command = self.command();
        command.args(["restore", "profile"]);
        if replace {
            command.arg("--replace");
        }
        command.output().unwrap()
    }

    fn open_tui(&self) -> Tui {
        let (master, slave) = terminal();
        let child = self
            .command()
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave)
            .spawn()
            .unwrap();
        Tui {
            master,
            child,
            output: Vec::new(),
            seen: 0,
        }
    }
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn assert_private_file(path: &Path, expected: &[u8]) {
    assert_eq!(fs::read(path).unwrap(), expected);
    assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o600);
}

#[test]
fn sets_and_replaces_a_whole_file_without_changing_the_input_bytes() {
    let config = Fixture::new();
    let destination = config.destination();
    assert!(config.list().contains("Not set"));

    let first = b"  synthetic-secret\0\xff\r\n";
    let output = success(config.set(first));
    assert!(!output.contains("synthetic-secret"));
    assert_private_file(&destination, first);
    assert!(config.list().contains("\tSet\n"));

    fs::write(&destination, b"").unwrap();
    assert!(config.list().contains("Not set"));
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o644)).unwrap();
    let old_metadata = fs::metadata(&destination).unwrap();
    let second = b" \n";
    success(config.set(second));
    assert_private_file(&destination, second);
    let new_metadata = fs::metadata(&destination).unwrap();
    assert_eq!(new_metadata.uid(), old_metadata.uid());
    assert_ne!(new_metadata.ino(), old_metadata.ino());
}

#[cfg(target_os = "linux")]
#[test]
fn cli_signals_abort_a_write_waiting_for_the_directory_lock() {
    use rustix::fs::{FlockOperation, flock};

    for signal in [libc::SIGINT, libc::SIGTERM] {
        let config = Fixture::new();
        let destination = config.destination();
        let parent = destination.parent().unwrap();
        fs::create_dir(parent).unwrap();
        fs::write(&destination, b"original").unwrap();
        let directory = File::open(parent).unwrap();
        flock(&directory, FlockOperation::LockExclusive).unwrap();

        let mut child = config
            .command()
            .args(["set", "token"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"replacement")
            .unwrap();

        // The transaction opens the parent once to pin it and again to lock it.
        let fds = format!("/proc/{}/fd", child.id());
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::read_dir(&fds)
            .unwrap()
            .flatten()
            .filter(|fd| fs::read_link(fd.path()).is_ok_and(|path| path == parent))
            .take(2)
            .count()
            < 2
        {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("set did not enter the write transaction");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // SAFETY: This PID is the live child which waits for the lock.
        assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("set did not cancel while waiting for the lock");
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(1), "signal {signal}: {output:?}");
        assert_failure(output, "write interrupted");
        assert_eq!(fs::read(&destination).unwrap(), b"original");
        assert_eq!(fs::read_dir(parent).unwrap().count(), 1);
    }
}

#[test]
fn restores_the_literal_fnox_path_key_as_an_exact_private_file() {
    let config = Fixture::with_config(FNOX_CONFIG);
    config.fnox_stub(r#"printf '  synthetic-fnox\000\377\r\n'"#);

    let output = success(config.restore(false));
    assert!(!output.contains("synthetic-fnox"));
    assert_private_file(&config.destination(), b"  synthetic-fnox\0\xff\r\n");
}

#[test]
fn fnox_status_uses_only_the_local_file_and_restore_requires_approval_even_when_empty() {
    let config = Fixture::with_config(FNOX_CONFIG);
    config.fnox_stub(r#"printf 'replacement\n'"#);
    let destination = config.destination();
    let called = config.root.join("fnox-called");

    assert_eq!(config.list(), "profile\tVPN profile\tNot set\n");
    fs::create_dir(destination.parent().unwrap()).unwrap();
    for existing in [b"".as_slice(), b"old"] {
        fs::write(&destination, existing).unwrap();
        let status = if existing.is_empty() {
            "Not set"
        } else {
            "Set"
        };
        assert_eq!(config.list(), format!("profile\tVPN profile\t{status}\n"));
        assert_failure(config.restore(false), "--replace");
        assert_eq!(fs::read(&destination).unwrap(), existing);
        assert!(!called.exists());
    }

    success(config.restore(true));
    assert_private_file(&destination, b"replacement\n");
}

#[test]
fn failed_fnox_fetches_name_the_source_and_leave_the_destination_intact() {
    let config = Fixture::with_config(FNOX_CONFIG);
    let destination = config.destination();
    fs::create_dir(destination.parent().unwrap()).unwrap();
    fs::write(&destination, b"old synthetic file").unwrap();

    for (body, reason) in [
        (None, "fnox executable not found"),
        (
            Some("printf 'secret-value\\n'; printf 'secret-value\\n' >&2; exit 42"),
            "exit status: 42",
        ),
        (Some("printf 'secret-value\\n' >&2"), "empty output"),
    ] {
        if let Some(body) = body {
            config.fnox_stub(body);
        }
        let output = config.restore(true);
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("~/.config/VPN $HOME; profile.conf")
        );
        assert_failure(output, reason);
        assert_eq!(fs::read(&destination).unwrap(), b"old synthetic file");
    }
}

#[test]
fn create_only_restore_keeps_a_file_created_during_the_fetch() {
    let config = Fixture::with_config(FNOX_CONFIG);
    let destination = config.destination();
    let parent = destination.parent().unwrap();
    fs::create_dir(parent).unwrap();
    config.fnox_stub(
        r#"
printf 'concurrent file' > "$XDG_CONFIG_HOME/secrets/token"
printf 'would-be replacement'
"#,
    );

    assert_failure(config.restore(false), "--replace");
    assert_eq!(fs::read(&destination).unwrap(), b"concurrent file");
    assert_eq!(fs::read_dir(parent).unwrap().count(), 1);
}

#[test]
fn refuses_empty_input_and_unsafe_destinations_without_revealing_secrets() {
    let config = Fixture::new();
    let destination = config.destination();
    let old = b"old-secret-value";
    let incoming = b"incoming-secret-value";
    success(config.set(old));
    assert_failure(config.set(b""), "secret input is empty");
    assert_private_file(&destination, old);

    let target = config.root.join("target");
    fs::rename(&destination, &target).unwrap();
    symlink(&target, &destination).unwrap();
    assert!(config.list().contains("Blocked"));
    assert_failure(config.set(incoming), "Blocked");
    assert_failure(config.clear("token"), "symbolic link");
    assert!(destination.is_symlink());
    assert_private_file(&target, old);

    fs::remove_file(&destination).unwrap();
    fs::create_dir(&destination).unwrap();
    assert!(config.list().contains("Blocked"));
    assert_failure(config.set(incoming), "expected a regular file");
    assert_failure(config.clear("token"), "expected a regular file");
    assert!(destination.is_dir());
    fs::remove_dir(&destination).unwrap();

    let outside = config.root.join("outside");
    fs::rename(destination.parent().unwrap(), &outside).unwrap();
    fs::rename(&target, outside.join("token")).unwrap();
    symlink(&outside, destination.parent().unwrap()).unwrap();
    assert!(config.list().contains("Blocked"));
    assert_failure(config.set(incoming), "symbolic link");
    assert_failure(config.clear("token"), "symbolic link");
    assert_private_file(&outside.join("token"), old);

    let output = config
        .command()
        .args(["set", "token", "incoming-secret-value"])
        .output()
        .unwrap();
    assert_failure(output, "usage");
}

fn assert_failure(output: Output, expected: &str) {
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(expected), "{stderr}");
    assert!(!stderr.contains("secret-value"));
    assert!(output.stdout.is_empty());
}

#[test]
fn cli_set_rejects_terminal_stdin() {
    let config = Fixture::new();
    let (_master, slave) = terminal();
    let output = config
        .command()
        .args(["set", "token"])
        .stdin(slave)
        .output()
        .unwrap();
    assert_failure(output, "non-terminal standard input");
    assert!(!config.destination().exists());
}

#[test]
fn validates_token_urls_for_manual_secrets_only() {
    for url in [
        "file:///tmp/token",
        "https:example.com",
        "https:/example.com/path://more",
        "https:///example.com",
        "https://",
        "https://[broken",
        "https://user:password@example.com",
        "https://example.com/with space",
    ] {
        let fixture = Fixture::new();
        let config = fixture.root.join("secsetter/config.toml");
        let input = fs::read_to_string(&config).unwrap();
        fs::write(config, format!("{input}\nurl = {url:?}\n")).unwrap();
        assert_failure(
            fixture.command().arg("validate").output().unwrap(),
            "url must be an absolute HTTP or HTTPS URL",
        );
    }
    let fixture = Fixture::with_config(&format!(
        "{FNOX_CONFIG}\nurl = 'https://example.com/tokens'"
    ));
    assert_failure(
        fixture.command().arg("validate").output().unwrap(),
        "url is only supported for secret-file and secret-field",
    );
    let field = Fixture::with_config(
        r#"
[[entries]]
id = "field"
label = "Token field"
kind = "secret-field"
destination = "$XDG_CONFIG_HOME/example.json"
format = "json"
key = ["token"]
url = "https://example.com/tokens"
"#,
    );
    assert_eq!(
        success(field.command().arg("validate").output().unwrap()),
        "Config is valid.\n"
    );
}

#[test]
fn tui_opens_token_pages_without_losing_secret_input() {
    let url = "https://example.com/tokens?name=demo&scope=read";
    let config = Fixture::with_config(&format!(
        r#"
[[entries]]
id = "token"
label = "Token file"
kind = "secret-file"
destination = "$XDG_CONFIG_HOME/secrets/token"
url = "{url}"
"#
    ));
    let opener = config.root.join(if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    });
    fs::write(
        &opener,
        format!(
            "#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = '{url}' ] || exit 42\n\
             printf '%s\\n' \"$1\" >> \"$XDG_CONFIG_HOME/opened\"\nexit 7\n"
        ),
    )
    .unwrap();
    fs::set_permissions(opener, fs::Permissions::from_mode(0o700)).unwrap();
    let opened = config.root.join("opened");
    let mut tui = config.open_tui();
    tui.read_through("o token page");
    assert!(!opened.exists());
    tui.send(b"o");
    tui.read_through("status:");
    tui.read_through("7");
    assert_eq!(fs::read_to_string(&opened).unwrap(), format!("{url}\n"));

    tui.send(b"\r");
    tui.read_through("\x1b[?1006h");
    tui.read_through("Open token page");
    tui.send(b"o\x1b[200~secret\x1b[201~");
    tui.read_through("******");
    tui.send(b"\x1b[<0;1;1M\x1b[<0;66;9M");
    tui.read_through("Could not open token page");
    assert_eq!(
        fs::read_to_string(&opened).unwrap(),
        format!("{url}\n").repeat(2)
    );

    tui.send(b"\x0f");
    tui.read_through("Could not open token page");
    assert_eq!(
        fs::read_to_string(&opened).unwrap(),
        format!("{url}\n").repeat(3)
    );

    tui.send(b"\r");
    tui.read_through("\x1b[?1006l");
    tui.read_through("✓ Set");
    assert_private_file(&config.destination(), b"osecret");
    tui.send(b"q");
    tui.expect_successful_exit();
}

#[test]
fn tui_masks_a_paste_and_waits_for_explicit_submission() {
    let config = Fixture::new();
    let destination = config.destination();
    let mut tui = config.open_tui();

    tui.read_through("○ Not set");
    tui.send(b"\r");
    tui.read_through("\x1b[8;20H");
    tui.read_through("Set secret");
    tui.read_through("Token file");
    tui.read_through("Destination:");
    tui.read_through("Paste or type a secret");
    tui.read_through("Enter submit   Esc cancel   Ctrl+J newline");

    // Both kinds of newline, Ctrl-C, and an escape sequence belong to the value.
    let pasted = "  synthetic-paste\r\nq\x03\x1b[A café\n";
    tui.send(b"\x1b[200~");
    tui.send(pasted.as_bytes());
    tui.send(b"\x1b[201~");
    tui.read_through("*****");
    assert!(!destination.exists());

    tui.send(b"\r");
    tui.read_through("✓ Set");
    assert_private_file(&destination, pasted.as_bytes());

    tui.send(b"\r");
    tui.read_through("Replace secret");
    tui.read_through("Enter submit   Esc cancel");
    tui.send(b"\x1b[200~discard-me\x1b[201~");
    tui.read_through("**********");
    tui.send(b"\x1b");
    tui.read_through("canceled");
    assert_private_file(&destination, pasted.as_bytes());

    tui.send(b"\r");
    tui.read_through("Enter submit   Esc cancel");
    tui.send(b"\r");
    tui.read_through("empty");
    assert_private_file(&destination, pasted.as_bytes());

    tui.send(b"q");
    tui.read_through("\x1b[?2004l");
    tui.expect_successful_exit();
    let output = String::from_utf8_lossy(&tui.output);
    assert!(!output.contains("synthetic-paste"));
    assert!(!output.contains("discard-me"));
}

#[test]
fn tui_uses_the_masked_prompt_for_yaml_and_keeps_the_original_on_cancel() {
    let config = Fixture::with_config(
        r#"
[[entries]]
id = "token"
label = "GitLab token"
kind = "secret-field"
destination = "$XDG_CONFIG_HOME/secrets/token"
format = "yaml"
key = ["token"]
"#,
    );
    let path = config.destination();
    let original = "token: 'old-synthetic-token' # keep\n";
    fs::create_dir(path.parent().unwrap()).unwrap();
    fs::write(&path, original).unwrap();
    let mut tui = config.open_tui();

    tui.read_through("✓ Set");
    tui.send(b"\r");
    tui.read_through("Enter submit   Esc cancel");
    tui.send(b"\x1b[200~discard-me\x1b[201~");
    tui.read_through("**********");
    tui.send(b"\x1b");
    tui.read_through("canceled");
    assert_eq!(fs::read_to_string(&path).unwrap(), original);

    tui.send(b"\r");
    tui.read_through("Enter submit   Esc cancel");
    tui.send(b"\x1b[200~new-synthetic-token\n\x1b[201~");
    tui.read_through("*******************");
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    tui.send(b"\r");
    tui.read_through("Set");
    let edited = fs::read_to_string(path).unwrap();
    assert!(edited.contains("# keep"));
    assert_eq!(
        oneyaml::Document::parse(edited)
            .unwrap()
            .get("token")
            .unwrap()
            .as_str()
            .as_deref(),
        Some("new-synthetic-token\n"),
    );

    tui.send(b"\x1b[3~");
    tui.read_through("Empty this field?");
    tui.read_through("[ Clear ]");
    tui.read_through("[ Cancel ]");
    tui.send(b"\x1b[D\r");
    tui.read_through("Cleared");
    assert_eq!(config.list(), "token\tGitLab token\tNot set\n");
    assert!(
        fs::read_to_string(config.destination())
            .unwrap()
            .contains("# keep")
    );

    tui.send(b"q");
    tui.read_through("\x1b[?2004l");
    tui.expect_successful_exit();
    let output = String::from_utf8_lossy(&tui.output);
    assert!(!output.contains("synthetic-token"));
    assert!(!output.contains("discard-me"));
}

#[test]
fn tui_clear_requires_approval_and_removes_the_whole_file() {
    let config = Fixture::new();
    success(config.set(b"synthetic"));
    let mut tui = config.open_tui();
    tui.read_through("Del clear");
    tui.send(b"\x1b[3~");
    tui.read_through("Remove this file?");
    tui.resize(32, 8);
    tui.read_through("Remove this file?");
    tui.read_through("…/token");
    tui.read_through("[Clear]");
    tui.read_through("›");
    tui.read_through("[Cancel]");
    tui.send(b"\r");
    tui.read_through("canceled");
    assert_private_file(&config.destination(), b"synthetic");

    tui.send(b"\x1b[3~");
    tui.read_through("Remove this file?");
    tui.send(b"y");
    tui.read_through("Cleared");
    assert!(!config.destination().exists());
    assert!(config.list().contains("Not set"));
    tui.send(b"\x1b[3~");
    tui.read_through("Nothing");
    assert!(!config.destination().exists());
    tui.send(b"q");
    tui.expect_successful_exit();
    assert_eq!(success(config.clear("token")), "Cleared\n");
}

#[test]
fn tui_preserves_the_selected_entry_and_masked_input_through_resize() {
    let config = Fixture::with_config(
        r#"
[[entries]]
id = "other"
label = "Other token"
kind = "secret-file"
destination = "$XDG_CONFIG_HOME/secrets/other"

[[entries]]
id = "token"
label = "Long token name that cannot fit in a compact row"
kind = "secret-file"
destination = "$XDG_CONFIG_HOME/secrets/token"
"#,
    );
    let mut tui = config.open_tui();
    tui.read_through("Selected entry");
    tui.send(b"\x1b[B");
    tui.read_through("› Long token");
    tui.send(b"\r");
    tui.read_through("Enter submit   Esc cancel");
    tui.send(b"\x1b[200~demo-abc\x1b[201~");
    tui.read_through("********");
    tui.send(b"\n\x1b[200~continued\x1b[201~");
    tui.read_through("*********");
    assert!(!config.destination().exists());

    tui.resize(160, 45);
    tui.read_through("\x1b[18;31H");
    tui.read_through("Set secret");
    tui.read_through("Destination:");
    tui.read_through("********");
    tui.read_through("*********");
    tui.read_through("Enter submit   Esc cancel");

    tui.resize(32, 8);
    tui.read_through("› Long token…");
    tui.read_through("○");
    tui.read_through("Not");
    tui.read_through("set");
    tui.read_through("\x1b[3;2H");
    tui.read_through("Set secret");
    tui.read_through("*********");
    tui.read_through("Enter OK  Ctrl+J line  Esc");

    tui.resize(24, 7);
    tui.read_through("Resize to 32×8");
    tui.read_through("\x1b[?25l");
    tui.send(b"\r");
    tui.read_through("\x1b[?25l");
    assert!(!config.destination().exists());

    tui.resize(100, 24);
    tui.read_through("Selected");
    tui.read_through("entry");
    tui.read_through("********");
    tui.read_through("*********");
    tui.read_through("Enter submit   Esc cancel");
    tui.send(b"\r");
    tui.read_through("✓ Set");
    assert_private_file(&config.destination(), b"demo-abc\ncontinued");
    assert!(!config.root.join("secrets/other").exists());
    assert!(!String::from_utf8_lossy(&tui.output).contains("demo-abc"));
    assert!(!String::from_utf8_lossy(&tui.output).contains("continued"));
    tui.send(b"q");
    tui.expect_successful_exit();
}

#[test]
fn tui_confirms_install_and_reinstall_and_preserves_focus_on_resize() {
    let config = Fixture::with_config(
        r#"
[[entries]]
id = "template"
label = "Example config"
kind = "template"
source = "example.toml"
destination = "$XDG_CONFIG_HOME/secrets/token"
"#,
    );
    let templates = config.root.join("secsetter/templates");
    fs::create_dir(&templates).unwrap();
    fs::write(templates.join("example.toml"), "enabled = true\n").unwrap();
    let destination = config.destination();
    let mut tui = config.open_tui();
    tui.read_through("○ Not installed");
    assert_failure(config.clear("template"), "clear does not match");

    tui.send(b"\r");
    tui.read_through("Install config?");
    tui.read_through("Destination");
    tui.read_through("[ Install ]");
    tui.read_through("[ Cancel ]");
    assert!(!destination.exists());

    tui.send(b"\t");
    tui.read_through("›");
    tui.read_through("[ Cancel ]");
    tui.resize(32, 10);
    tui.read_through("Install config?");
    tui.read_through("…/token");
    tui.read_through("[Install]");
    tui.read_through("›");
    tui.read_through("[Cancel]");
    tui.resize(32, 8);
    tui.read_through("Install config?");
    tui.read_through("[Install]");
    tui.read_through("›");
    tui.read_through("[Cancel]");
    tui.send(b"\r");
    tui.read_through("canceled");
    assert!(!destination.exists());

    tui.send(b"\r");
    tui.read_through("›");
    tui.read_through("[Install]");
    tui.send(b"\r");
    tui.read_through("✓ Installed");
    assert_eq!(
        fs::read_to_string(&destination).unwrap(),
        "enabled = true\n"
    );

    fs::write(&destination, "enabled = false\n").unwrap();
    tui.send(b"\r");
    tui.read_through("Reinstall from template?");
    tui.read_through("[Cancel]");
    tui.send(b"n");
    tui.read_through("canceled");
    assert_eq!(
        fs::read_to_string(&destination).unwrap(),
        "enabled = false\n"
    );

    tui.send(b"\r");
    tui.read_through("Reinstall from template?");
    tui.read_through("[Reinstall]");
    tui.send(b"\r");
    tui.read_through("Installed");
    assert_eq!(fs::read_to_string(destination).unwrap(), "enabled = true\n");
    tui.send(b"q");
    tui.expect_successful_exit();
}

#[test]
fn tui_confirms_first_fnox_restore_and_replacement() {
    let config = Fixture::with_config(FNOX_CONFIG);
    config.fnox_stub(r#"printf 'synthetic-fnox\n'"#);
    let destination = config.destination();
    let called = config.root.join("fnox-called");
    let mut tui = config.open_tui();

    tui.read_through("○ Not set");
    tui.send(b"\r");
    tui.read_through("Restore from fnox?");
    tui.read_through("[ Restore ]");
    assert!(!called.exists() && !destination.exists());
    tui.send(b"n");
    tui.read_through("canceled");
    assert!(!called.exists() && !destination.exists());

    tui.send(b"\r");
    tui.read_through("Restore from fnox?");
    tui.read_through("[ Restore ]");
    fs::create_dir(destination.parent().unwrap()).unwrap();
    fs::write(&destination, b"new local file").unwrap();
    tui.send(b"\r");
    tui.read_through("exists;");
    assert!(!called.exists());
    assert_eq!(fs::read(&destination).unwrap(), b"new local file");
    fs::remove_file(&destination).unwrap();

    tui.send(b"\r");
    tui.read_through("Restore from fnox?");
    tui.read_through("[ Restore ]");
    tui.send(b"\r");
    tui.read_through("Restored");
    assert_private_file(&destination, b"synthetic-fnox\n");

    fs::write(&destination, b"").unwrap();
    fs::remove_file(&called).unwrap();
    tui.send(b"\r");
    tui.read_through("Replace from fnox?");
    tui.read_through("[ Replace ]");
    tui.read_through("[ Cancel ]");
    assert!(!called.exists());
    tui.send(b"\x1b[C\r");
    tui.read_through("canceled");
    assert!(!called.exists());
    assert_eq!(fs::read(&destination).unwrap(), b"");

    tui.send(b"\r");
    tui.read_through("Replace from fnox?");
    tui.read_through("[ Replace ]");
    tui.send(b"\x1b[C\x1b[D\r");
    tui.read_through("✓ Set");
    tui.read_through("Restored");
    assert_private_file(&destination, b"synthetic-fnox\n");
    assert!(!String::from_utf8_lossy(&tui.output).contains("synthetic-fnox"));
    tui.send(b"q");
    tui.expect_successful_exit();

    fs::remove_file(&called).unwrap();
    assert_eq!(success(config.clear("profile")), "Cleared\n");
    assert!(!destination.exists());
    assert!(!called.exists());
    success(config.restore(false));
    assert_private_file(&destination, b"synthetic-fnox\n");
}

fn terminal() -> (File, File) {
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
    fcntl_setfd(&master, FdFlags::CLOEXEC).unwrap();
    let flags = rustix::fs::fcntl_getfl(&master).unwrap();
    rustix::fs::fcntl_setfl(&master, flags | OFlags::NONBLOCK).unwrap();
    grantpt(&master).unwrap();
    unlockpt(&master).unwrap();
    let slave = open(
        ptsname(&master, Vec::new()).unwrap().as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    tcsetwinsize(
        &slave,
        Winsize {
            ws_row: 24,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    (master.into(), slave.into())
}

struct Tui {
    master: File,
    child: Child,
    output: Vec<u8>,
    seen: usize,
}

impl Tui {
    fn send(&mut self, input: &[u8]) {
        self.master.write_all(input).unwrap();
    }

    fn resize(&self, width: u16, height: u16) {
        tcsetwinsize(
            &self.master,
            Winsize {
                ws_row: height,
                ws_col: width,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .unwrap();
        // This PTY does not make the child a foreground process group.
        // SAFETY: The child is still running and SIGWINCH is a valid signal.
        assert_eq!(
            unsafe { libc::kill(self.child.id() as i32, libc::SIGWINCH) },
            0
        );
    }

    fn read_through(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(index) = self.output[self.seen..]
                .windows(text.len())
                .position(|bytes| bytes == text.as_bytes())
            {
                self.seen += index + text.len();
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = Timespec {
                tv_sec: remaining.as_secs() as _,
                tv_nsec: remaining.subsec_nanos() as _,
            };
            let mut poll_fds = [PollFd::new(&self.master, PollFlags::IN)];
            assert!(
                poll(&mut poll_fds, Some(&timeout)).unwrap() != 0,
                "timed out waiting for {text:?} in {:?}",
                String::from_utf8_lossy(&self.output)
            );
            let mut buffer = [0; 1024];
            let read = match self.master.read(&mut buffer) {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) => panic!("read terminal while waiting for {text:?}: {error}"),
            };
            assert!(read > 0, "terminal closed before showing {text:?}");
            self.output.extend_from_slice(&buffer[..read]);
        }
    }

    fn expect_successful_exit(&mut self) {
        let status = self.wait_for_exit().expect("SecSetter did not exit");
        assert!(status.success(), "{status}");
    }

    fn wait_for_exit(&mut self) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let mut buffer = [0; 4096];
            if let Ok(read) = self.master.read(&mut buffer) {
                self.output.extend_from_slice(&buffer[..read]);
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.wait_for_exit();
    }
}
