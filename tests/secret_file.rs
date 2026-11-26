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

        // Wait until the child enters the transaction, with the directory still locked.
        let fds = format!("/proc/{}/fd", child.id());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fs::read_dir(&fds)
            .unwrap()
            .flatten()
            .any(|fd| fs::read_link(fd.path()).is_ok_and(|path| path == parent))
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

        assert_failure(child.wait_with_output().unwrap(), "write interrupted");
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
    assert!(destination.is_symlink());
    assert_private_file(&target, old);

    fs::remove_file(&destination).unwrap();
    fs::create_dir(&destination).unwrap();
    assert!(config.list().contains("Blocked"));
    assert_failure(config.set(incoming), "expected a regular file");
    assert!(destination.is_dir());
    fs::remove_dir(&destination).unwrap();

    let outside = config.root.join("outside");
    fs::rename(destination.parent().unwrap(), &outside).unwrap();
    fs::rename(&target, outside.join("token")).unwrap();
    symlink(&outside, destination.parent().unwrap()).unwrap();
    assert!(config.list().contains("Blocked"));
    assert_failure(config.set(incoming), "symbolic link");
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
fn tui_masks_a_paste_and_waits_for_explicit_submission() {
    let config = Fixture::new();
    let destination = config.destination();
    let mut tui = config.open_tui();

    tui.read_through("Token file — Not set");
    tui.send(b"\r");
    tui.read_through("Secret (Enter submit, Esc cancel): ");

    // Both kinds of newline, Ctrl-C, and an escape sequence belong to the value.
    let pasted = "  synthetic-paste\r\nq\x03\x1b[A café\n";
    tui.send(b"\x1b[200~");
    tui.send(pasted.as_bytes());
    tui.send(b"\x1b[201~");
    tui.read_through("Secret (Enter submit, Esc cancel): *");
    assert!(!destination.exists());

    tui.send(b"\r");
    tui.read_through("Token file — Set");
    assert_private_file(&destination, pasted.as_bytes());

    tui.send(b"\r");
    tui.read_through("Secret (Enter submit, Esc cancel): ");
    tui.send(b"discard-me");
    tui.read_through("Secret (Enter submit, Esc cancel): **********");
    tui.send(b"\x1b");
    tui.read_through("Set canceled");
    assert_private_file(&destination, pasted.as_bytes());

    tui.send(b"\r");
    tui.read_through("Secret (Enter submit, Esc cancel): ");
    tui.send(b"\r");
    tui.read_through("secret input is empty");
    assert_private_file(&destination, pasted.as_bytes());

    tui.send(b"q");
    tui.read_through("\x1b[?2004l");
    assert!(tui.child.wait().unwrap().success());
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

    tui.read_through("GitLab token — Set");
    tui.send(b"\r");
    tui.read_through("Secret (Enter submit, Esc cancel): ");
    tui.send(b"discard-me");
    tui.read_through("Secret (Enter submit, Esc cancel): **********");
    tui.send(b"\x1b");
    tui.read_through("Set canceled");
    assert_eq!(fs::read_to_string(&path).unwrap(), original);

    tui.send(b"\r");
    tui.read_through("Secret (Enter submit, Esc cancel): ");
    tui.send(b"\x1b[200~new-synthetic-token\n\x1b[201~");
    tui.read_through("Secret (Enter submit, Esc cancel): ********************");
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    tui.send(b"\r");
    tui.read_through("GitLab token — Set");
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

    tui.send(b"q");
    tui.read_through("\x1b[?2004l");
    assert!(tui.child.wait().unwrap().success());
    let output = String::from_utf8_lossy(&tui.output);
    assert!(!output.contains("synthetic-token"));
    assert!(!output.contains("discard-me"));
}

#[test]
fn tui_restores_a_missing_file_and_asks_before_replacing_an_empty_file() {
    let config = Fixture::with_config(FNOX_CONFIG);
    config.fnox_stub(r#"printf 'synthetic-fnox\n'"#);
    let destination = config.destination();
    let called = config.root.join("fnox-called");
    let mut tui = config.open_tui();

    tui.read_through("VPN profile — Not set");
    tui.send(b"\r");
    tui.read_through("Restored");
    assert_private_file(&destination, b"synthetic-fnox\n");

    fs::write(&destination, b"").unwrap();
    fs::remove_file(&called).unwrap();
    tui.send(b"\r");
    tui.read_through("Replace VPN profile? [y/N]");
    assert!(!called.exists());
    tui.send(b"\r");
    tui.read_through("Replace canceled");
    assert!(!called.exists());
    assert_eq!(fs::read(&destination).unwrap(), b"");

    tui.send(b"\r");
    tui.read_through("Replace VPN profile? [y/N]");
    tui.send(b"y");
    tui.read_through("VPN profile — Set");
    tui.read_through("Restored");
    assert_private_file(&destination, b"synthetic-fnox\n");
    assert!(!String::from_utf8_lossy(&tui.output).contains("synthetic-fnox"));
    tui.send(b"q");
    assert!(tui.child.wait().unwrap().success());
}

fn terminal() -> (File, File) {
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
    fcntl_setfd(&master, FdFlags::CLOEXEC).unwrap();
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
            let read = self.master.read(&mut buffer).unwrap();
            assert!(read > 0, "terminal closed before showing {text:?}");
            self.output.extend_from_slice(&buffer[..read]);
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
