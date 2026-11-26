use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus};
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

use rustix::termios::{LocalModes, tcgetattr};

const LOGIN: &str = r#"
[[entries]]
id = "demo"
label = "Synthetic sign-in"
kind = "login"
command = ["sh", "-c", "printf '%s\\n' \"$1\"; exit 43", "demo", "$HOME; exit 0"]
"#;

const TEMPLATE: &str = r#"
[[entries]]
id = "example-template"
label = "Example config"
kind = "template"
source = "example.toml"
destination = "$XDG_CONFIG_HOME/example.toml"
"#;

struct ConfigDir(tempfile::TempDir);

impl ConfigDir {
    fn new(config: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("secsetter")).unwrap();
        fs::write(directory.path().join("secsetter/config.toml"), config).unwrap();
        Self(directory)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_secsetter"));
        command.env("XDG_CONFIG_HOME", self.0.path());
        command
    }
}

#[test]
fn cli_passes_literal_arguments_and_returns_the_command_exit_code() {
    let config = ConfigDir::new(LOGIN);
    let output = config.command().args(["login", "demo"]).output().unwrap();
    assert_eq!(output.status.code(), Some(43), "{output:?}");
    assert_eq!(output.stdout, b"$HOME; exit 0\n");

    let list = config.command().arg("list").output().unwrap();
    assert!(list.status.success(), "{list:?}");
    assert_eq!(list.stdout, b"demo\tSynthetic sign-in\n");
}

#[test]
fn refuses_invalid_commands_before_running_a_login() {
    for invalid in [
        r#"command = []"#,
        r#"command = [""]"#,
        r#"command = ["sh", "\u0000"]"#,
        "privileged = true\ncommand = [\"sh\"]",
        "",
    ] {
        let entry = LOGIN.split_once("command =").unwrap().0;
        let config = ConfigDir::new(&format!("{entry}{invalid}"));
        let output = config.command().args(["login", "demo"]).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid"));
    }
}

#[test]
fn login_rejects_unknown_ids_inapplicable_entries_and_other_kinds() {
    let other_platform = if cfg!(target_os = "linux") {
        "macos"
    } else {
        "linux"
    };
    let login = LOGIN.replace(
        "command =",
        &format!("platforms = [\"{other_platform}\"]\ncommand ="),
    );
    let config = ConfigDir::new(&format!("{login}{TEMPLATE}"));
    for (id, reason) in [
        ("missing", "unknown entry ID"),
        ("demo", "does not apply"),
        ("example-template", "does not match"),
    ] {
        let output = config.command().args(["login", id]).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(reason));
    }
}

#[test]
fn runs_a_configured_script_only_with_an_explicit_interpreter() {
    let entry = LOGIN.split_once("command =").unwrap().0;
    let config = ConfigDir::new(&format!(r#"{entry}command = ["bare-script"]"#));
    let script = config.0.path().join("bare-script");
    fs::write(&script, "echo 'must not run'\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();

    let mut command = config.command();
    command.env("PATH", config.0.path());
    let output = command.args(["login", "demo"]).output().unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");

    fs::write(&script, "#!/bin/sh\nprintf 'script ran\\n'\n").unwrap();
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"script ran\n");
}

const INTERACTIVE_LOGIN: &str = r#"
[[entries]]
id = "demo"
label = "Synthetic sign-in"
kind = "login"
command = ["/bin/sh", "-c", '''
test -t 0 && test -t 1 && test -t 2 || exit 90
printf 'Synthetic prompt: ' >/dev/tty
IFS= read -r reply </dev/tty || exit 91
test "$reply" = "synthetic answer" || exit 42
printf 'Read answer\n'
''']
"#;

#[test]
fn tui_returns_to_the_list_after_an_interactive_login_succeeds() {
    let config = ConfigDir::new(&format!("{INTERACTIVE_LOGIN}{TEMPLATE}"));
    let mut pty = Terminal::launch(config.command());
    pty.expect("> Example config — Not installed");
    assert!(pty.expect("Synthetic sign-in").contains("Login"));

    pty.send(b"\x1b[B");
    pty.expect("> Synthetic sign-in");
    pty.send(b"\r");
    assert!(pty.expect("Synthetic prompt: ").contains("\x1b[?1049l"));
    pty.send(b"synthetic answer\n");
    let result = pty.expect("Login completed");
    assert!(result.contains("Read answer"));
    assert!(result.contains("\x1b[?1049h") && result.contains("Synthetic sign-in"));
    pty.quit();
}

#[test]
fn tui_returns_to_the_list_with_the_command_failure_status() {
    let config = ConfigDir::new(INTERACTIVE_LOGIN);
    let mut pty = Terminal::launch(config.command());
    pty.expect("Synthetic sign-in");
    pty.send(b"\r");
    pty.expect("Synthetic prompt: ");

    pty.send(b"wrong answer\n");
    let result = pty.expect("Login failed: exit status: 42");
    assert!(result.contains("\x1b[?1049h") && result.contains("Synthetic sign-in"));
    pty.quit();
}

#[test]
fn tui_returns_to_the_list_when_the_executable_is_missing() {
    let entry = LOGIN.split_once("command =").unwrap().0;
    let config = ConfigDir::new(&format!(r#"{entry}command = ["./missing-login-command"]"#));
    let mut pty = Terminal::launch(config.command());
    pty.expect("Synthetic sign-in");
    pty.send(b"\r");

    let result = pty.expect("Could not run login");
    assert!(result.contains("\x1b[?1049l") && result.contains("\x1b[?1049h"));
    assert!(result.contains("Synthetic sign-in"));
    pty.quit();
}

#[test]
fn tui_returns_to_the_list_after_ctrl_c_and_restores_terminal_modes() {
    let config = ConfigDir::new(&INTERACTIVE_LOGIN.replace(
        "printf 'Synthetic prompt",
        "stty -echo </dev/tty\nprintf 'Synthetic prompt",
    ));
    let mut pty = Terminal::launch(config.command());
    pty.expect("Synthetic sign-in");
    pty.send(b"\r");
    pty.expect("Synthetic prompt: ");
    let prompt_modes = tcgetattr(&pty.slave).unwrap().local_modes;
    assert!(prompt_modes.contains(LocalModes::ICANON | LocalModes::ISIG));
    assert!(!prompt_modes.contains(LocalModes::ECHO));

    pty.send(b"\x03");
    let result = pty.expect("Login interrupted");
    assert!(result.contains("\x1b[?1049h") && result.contains("Synthetic sign-in"));
    pty.quit();
    let restored_modes = tcgetattr(&pty.slave).unwrap().local_modes;
    assert!(restored_modes.contains(LocalModes::ECHO | LocalModes::ICANON | LocalModes::ISIG));
}

#[test]
fn cli_runs_in_the_current_terminal_and_preserves_interrupt_status() {
    let config = ConfigDir::new(INTERACTIVE_LOGIN);
    let mut command = config.command();
    command.args(["login", "demo"]);
    let mut pty = Terminal::launch(command);
    pty.expect("Synthetic prompt: ");
    pty.send(b"\x03");
    assert_eq!(pty.wait().signal(), Some(libc::SIGINT));
}

struct Terminal {
    child: Child,
    master: File,
    slave: File,
    unread: Vec<u8>,
}

impl Terminal {
    fn launch(mut command: Command) -> Self {
        let (mut master, mut slave) = (-1, -1);
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: The output pointers are valid, and openpty returns two new descriptors.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &raw mut size,
                )
            },
            0
        );
        // SAFETY: The successful openpty call returned owned descriptors.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        for fd in [&master, &slave] {
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC).unwrap();
        }
        command
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        // SAFETY: These syscalls run after Command installs the slave on stdin, without locks or allocation.
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid()?;
                let stdin = std::os::fd::BorrowedFd::borrow_raw(libc::STDIN_FILENO);
                rustix::process::ioctl_tiocsctty(stdin)?;
                Ok(())
            });
        }
        Self {
            child: command.spawn().unwrap(),
            master,
            slave,
            unread: Vec::new(),
        }
    }

    fn expect(&mut self, text: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(index) = self
                .unread
                .windows(text.len())
                .position(|bytes| bytes == text.as_bytes())
            {
                let received: Vec<_> = self.unread.drain(..index + text.len()).collect();
                return String::from_utf8_lossy(&received).into_owned();
            }
            let timeout = deadline.saturating_duration_since(Instant::now());
            let mut poll = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: The descriptor is live, and the pointer refers to one pollfd.
            let ready = unsafe { libc::poll(&mut poll, 1, timeout.as_millis() as i32) };
            assert!(
                !timeout.is_zero() && ready == 1 && poll.revents & libc::POLLIN != 0,
                "expected {text:?}, got {:?}",
                String::from_utf8_lossy(&self.unread)
            );
            let mut chunk = [0; 4096];
            let count = self.master.read(&mut chunk).unwrap();
            assert!(count > 0, "terminal closed before {text:?}");
            self.unread.extend_from_slice(&chunk[..count]);
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }

    fn quit(&mut self) {
        self.send(b"q");
        self.expect("\x1b[?1049l");
        assert!(self.wait().success());
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "SecSetter did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            // SAFETY: The live child started a fresh session and process group.
            unsafe { libc::killpg(self.child.id() as i32, libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
}
