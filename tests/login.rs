use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
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
commands = [["sh", "-c", "printf '%s\\n' \"$1\"; exit 43", "demo", "$HOME; exit 0"]]
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
        command.env("XDG_CONFIG_HOME", self.0.path().canonicalize().unwrap());
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
fn cli_runs_steps_in_order_and_stops_at_the_first_failure() {
    let entry = LOGIN.split_once("commands =").unwrap().0;
    let config = ConfigDir::new(&format!(
        r#"{entry}commands = [
  ["/bin/sh", "-c", "printf 'first\\n'"],
  ["/bin/sh", "-c", "printf 'second\\n'; exit 19"],
  ["/bin/sh", "-c", "printf 'third\\n'"],
]"#
    ));
    let output = config.command().args(["login", "demo"]).output().unwrap();
    assert_eq!(output.status.code(), Some(19), "{output:?}");
    assert_eq!(output.stdout, b"first\nsecond\n");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Login failed at step 2: exit status: 19")
    );
}

#[test]
fn login_status_requires_all_local_checks_and_never_runs_login_steps() {
    let config = ConfigDir::new(&format!(
        r#"{LOGIN}
ready_when = [
  {{ kind = "field-set", path = "$XDG_CONFIG_HOME/account.json", format = "json", key = ["account", "user"] }},
  {{ kind = "file-exists", path = "$XDG_CONFIG_HOME/profile" }},
]
"#
    ));
    let account = config.0.path().join("account.json");
    let profile = config.0.path().join("profile");
    let list = || {
        let output = config.command().arg("list").output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty());
        String::from_utf8(output.stdout).unwrap()
    };
    let needed = "demo\tSynthetic sign-in\tLogin needed\n";

    assert_eq!(list(), needed);
    fs::write(&account, r#"{"account":{"user":"synthetic-user"}}"#).unwrap();
    assert_eq!(list(), needed);
    fs::write(&profile, "").unwrap();
    assert_eq!(list(), "demo\tSynthetic sign-in\tLogin configured\n");
    for contents in [r#"{"account":{"user":""}}"#, r#"{"account":{}}"#] {
        fs::write(&account, contents).unwrap();
        assert_eq!(list(), needed);
    }
    fs::write(&account, r#"{"account":{"user":"do-not-display""#).unwrap();
    let status = list();
    assert!(status.starts_with("demo\tSynthetic sign-in\tUnknown:"));
    assert!(status.contains("account.json"));
    assert!(!status.contains("do-not-display"));
    fs::remove_file(&profile).unwrap();
    assert_eq!(list(), needed);

    fs::write(&account, r#"{"account":{"user":"synthetic-user"}}"#).unwrap();
    symlink(&account, &profile).unwrap();
    let status = list();
    assert!(status.contains("Unknown:") && status.contains("symbolic link"));
}

#[test]
fn rejects_invalid_login_checks_at_config_load() {
    for check in [
        r#"{ kind = "file-exists", path = "relative/profile" }"#,
        r#"{ kind = "field-set", path = "~/profile", format = "toml", key = [] }"#,
        r#"{ kind = "field-set", path = "~/profile", format = "toml" }"#,
    ] {
        let config = ConfigDir::new(&format!("{LOGIN}\nready_when = [{check}]"));
        let output = config.command().arg("list").output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid"));
    }
}

#[test]
fn refuses_invalid_commands_before_running_a_login() {
    for invalid in [
        r#"commands = []"#,
        r#"commands = [["/bin/echo", "must not run"], []]"#,
        r#"commands = [[""]]"#,
        r#"commands = [["sh", "\u0000"]]"#,
        "privileged = true\ncommands = [[\"sh\"]]",
        r#"command = ["sh"]"#,
        "",
    ] {
        let entry = LOGIN.split_once("commands =").unwrap().0;
        let config = ConfigDir::new(&format!("{entry}{invalid}"));
        let output = config.command().args(["login", "demo"]).output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
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
        "commands =",
        &format!("platforms = [\"{other_platform}\"]\ncommands ="),
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
    let entry = LOGIN.split_once("commands =").unwrap().0;
    let config = ConfigDir::new(&format!(r#"{entry}commands = [["bare-script"]]"#));
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
commands = [
["/bin/sh", "-c", '''
test -t 0 && test -t 1 && test -t 2 || exit 90
printf 'Synthetic prompt: ' >/dev/tty
IFS= read -r reply </dev/tty || exit 91
test "$reply" = "synthetic answer" || exit 42
printf 'Read answer\n'
'''],
["/bin/sh", "-c", '''
printf 'Next prompt: ' >/dev/tty
IFS= read -r reply </dev/tty || exit 91
test "$reply" = "second answer" || exit 44
: > "$XDG_CONFIG_HOME/ready-marker" || exit 45
printf 'Read second answer\n'
'''],
]
"#;

#[test]
fn tui_returns_to_the_list_after_an_interactive_login_succeeds() {
    let config = ConfigDir::new(&format!(
        r#"{INTERACTIVE_LOGIN}
ready_when = [{{ kind = "file-exists", path = "$XDG_CONFIG_HOME/ready-marker" }}]
{TEMPLATE}"#
    ));
    let mut pty = Terminal::launch(config.command());
    assert!(pty.expect("○ Not installed").contains("\x1b[?2004h"));
    assert!(pty.expect("sign-in").contains("Login"));
    pty.expect("needed");

    pty.send(b"\x1b[B");
    pty.expect("› Synthetic sign-in");
    pty.send(b"\r");
    let suspended = pty.expect("Synthetic prompt: ");
    assert!(suspended.contains("\x1b[?2004l") && suspended.contains("\x1b[?1049l"));
    pty.send(b"synthetic answer\n");
    let between = pty.expect("Next prompt: ");
    assert!(between.contains("Read answer") && !between.contains("\x1b[?1049h"));
    pty.send(b"second answer\n");
    let result = pty.expect("completed");
    assert!(result.contains("Read second answer"));
    assert!(result.contains("configured"));
    assert!(result.contains("\x1b[?2004h"));
    assert!(result.contains("\x1b[?1049h") && result.contains("Synthetic sign-in"));
    pty.quit();
}

#[test]
fn tui_keeps_login_failure_output_visible_until_enter() {
    let config = ConfigDir::new(&INTERACTIVE_LOGIN.replace(
        "|| exit 42",
        "|| { printf 'Synthetic login failure\\n' >&2; exit 42; }",
    ));
    let mut pty = Terminal::launch(config.command());
    pty.expect("Synthetic sign-in");
    pty.send(b"\r");
    pty.expect("Synthetic prompt: ");

    pty.send(b"wrong answer\n");
    let failure = pty.expect("Press Enter to return to SecSetter.\r\n");
    assert!(failure.contains("Synthetic login failure"));
    assert!(failure.contains("Login failed at step 1 of 2 (exit code 42)."));
    assert!(!failure.contains("\x1b[?1049h") && !failure.contains("Next prompt:"));

    pty.send(b"q");
    assert!(pty.unread.is_empty());
    let mut poll = libc::pollfd {
        fd: pty.master.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: The descriptor is live, and the pointer refers to one pollfd.
    assert_eq!(unsafe { libc::poll(&mut poll, 1, 100) }, 0);

    pty.send(b"\r");
    let result = pty.expect("failed");
    assert!(result.contains("\x1b[?1049h") && result.contains("Synthetic sign-in"));
    pty.expect("42");
    pty.quit();
}

#[test]
fn tui_returns_to_the_list_when_the_executable_is_missing() {
    let entry = LOGIN.split_once("commands =").unwrap().0;
    let config = ConfigDir::new(&format!(
        r#"{entry}commands = [["./missing-login-command"]]"#
    ));
    let mut pty = Terminal::launch(config.command());
    pty.expect("Synthetic sign-in");
    pty.send(b"\r");

    let failure = pty.expect("Press Enter to return to SecSetter.");
    assert!(failure.contains("Could not run login step 1 of 1:"));
    assert!(failure.contains("\x1b[?1049l") && !failure.contains("\x1b[?1049h"));
    pty.send(b"\x03");
    let result = pty.expect("login");
    assert!(result.contains("\x1b[?1049h"));
    assert!(result.contains("Synthetic sign-in"));
    pty.quit();
}

#[test]
fn tui_stops_on_ctrl_c_even_if_the_step_exits_successfully_and_restores_terminal_modes() {
    let config = ConfigDir::new(&INTERACTIVE_LOGIN.replace(
        "printf 'Synthetic prompt",
        "trap 'echo Caught interrupt; exit 0' INT\n\
         if [ ! -f \"$XDG_CONFIG_HOME/login-started\" ]; then\n\
         : > \"$XDG_CONFIG_HOME/login-started\"\n\
         stty -echo </dev/tty\n\
         fi\n\
         printf 'Synthetic prompt",
    ));
    let mut pty = Terminal::launch(config.command());
    pty.expect("Synthetic sign-in");
    pty.send(b"\r");
    pty.expect("Synthetic prompt: ");
    let prompt_modes = tcgetattr(&pty.slave).unwrap().local_modes;
    assert!(prompt_modes.contains(LocalModes::ICANON | LocalModes::ISIG));
    assert!(!prompt_modes.contains(LocalModes::ECHO));

    pty.send(b"\x03");
    let result = pty.expect("interrupted");
    assert!(result.contains("Caught interrupt"));
    assert!(result.contains("\x1b[?1049h") && result.contains("Synthetic sign-in"));
    // Check the next handoff before macOS revokes the session's terminal on exit.
    pty.send(b"\r");
    pty.expect("Synthetic prompt: ");
    let restored_modes = tcgetattr(&pty.slave).unwrap().local_modes;
    assert!(restored_modes.contains(LocalModes::ECHO | LocalModes::ICANON | LocalModes::ISIG));
    pty.send(b"synthetic answer\n");
    pty.expect("Next prompt: ");
    pty.send(b"second answer\n");
    pty.expect("completed");
    pty.quit();
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

#[cfg(target_os = "linux")]
#[test]
#[ignore = "run inside an isolated Linux user namespace with UID and GID 1 mapped"]
fn fake_sudo_uses_the_controlling_terminal_and_secsetter_recovers_from_refusal_or_ctrl_c() {
    let config = ConfigDir::new(
        r#"
[[entries]]
id = "token"
label = "Synthetic privileged token"
kind = "secret-file"
destination = "$XDG_CONFIG_HOME/token"
privileged = true
owner = "root"
group = "root"
"#,
    );
    let base = config.0.path();
    fs::set_permissions(base, fs::Permissions::from_mode(0o755)).unwrap();
    let fake_sudo = base.join("fake-sudo");
    fs::write(
        &fake_sudo,
        r#"#!/bin/sh
noninteractive=false
if [ "$1" = -n ]; then
    noninteractive=true
    shift
fi
[ "$#" = 10 ] && [ "$1" = -u ] && [ "$2" = root ] && [ "$3" = -- ] &&
    [ "$5" = __write ] && [ -z "$(cat)" ] || exit 90
if "$noninteractive"; then
    printf 'Synthetic noninteractive refusal\n' >&2
    exit 41
fi
printf 'Synthetic sudo prompt: ' >/dev/tty
IFS= read -r answer </dev/tty || exit 91
[ "$answer" = 'no' ] || exit 92
exit 42
"#,
    )
    .unwrap();
    fs::set_permissions(&fake_sudo, fs::Permissions::from_mode(0o755)).unwrap();

    let isolated = || {
        let mut command = Command::new("/usr/bin/unshare");
        command
            .args(["--mount", "--propagation", "private", "/bin/sh", "-c"])
            .arg(
                r#"
                /usr/bin/mount --bind "$1" /usr/bin/sudo || exit 93
                shift
                exec /usr/bin/setpriv --reuid=1 --regid=1 --clear-groups "$@"
                "#,
            )
            .arg("isolated-sudo")
            .arg(&fake_sudo)
            .arg(env!("CARGO_BIN_EXE_secsetter"))
            .env("XDG_CONFIG_HOME", base);
        command
    };
    let mut pty = Terminal::launch(isolated());
    pty.expect("○ Not set");
    for (answer, failure) in [
        (b"no\n".as_slice(), "status:"),
        (b"\x03".as_slice(), "signal:"),
    ] {
        pty.send(b"\r");
        pty.expect("Enter submit   Esc cancel");
        pty.send(b"synthetic payload\r");
        let suspended = pty.expect("Synthetic sudo prompt: ");
        assert!(suspended.contains("\x1b[?2004l") && suspended.contains("\x1b[?1049l"));
        pty.send(answer);
        pty.expect("\x1b[?1049h");
        assert!(pty.expect(failure).contains("Not set"));
        assert!(!base.join("token").exists());
    }
    pty.quit();

    let mut command = isolated();
    use std::process::Stdio;
    command
        .args(["set", "token"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: Only the async-signal-safe setsid syscall runs before exec.
    unsafe { command.pre_exec(|| rustix::process::setsid().map(|_| ()).map_err(Into::into)) };
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"synthetic payload\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Synthetic noninteractive refusal"),
        "{stderr}"
    );
    assert!(stderr.contains("exit status: 41"), "{stderr}");
    assert!(!stderr.contains("synthetic payload"));
    assert!(!base.join("token").exists());
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
        let flags = rustix::fs::fcntl_getfl(&master).unwrap();
        rustix::fs::fcntl_setfl(&master, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
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
            let count = match self.master.read(&mut chunk) {
                Ok(count) => count,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(error) => panic!("read terminal while waiting for {text:?}: {error}"),
            };
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
        self.wait_for_exit().expect("SecSetter did not exit")
    }

    fn wait_for_exit(&mut self) -> Option<ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            // macOS drains the controlling terminal before the session leader can exit.
            let _ = self.master.read(&mut [0; 4096]);
            thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            // SAFETY: The live child started a fresh session and process group.
            unsafe { libc::killpg(self.child.id() as i32, libc::SIGKILL) };
            let _ = self.wait_for_exit();
        }
    }
}
