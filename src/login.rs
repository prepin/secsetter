use std::ffi::{CStr, CString, c_char};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitCode, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::{env, io, mem, ptr};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const CHECK_INTERVAL: Duration = Duration::from_millis(25);

pub(crate) struct InterruptGuard(libc::sigaction);

impl InterruptGuard {
    pub fn new() -> io::Result<Self> {
        extern "C" fn interrupt(_: libc::c_int) {
            INTERRUPTED.store(true, Ordering::Relaxed);
        }

        INTERRUPTED.store(false, Ordering::Relaxed);
        // The handler catches SIGINT in the parent and resets to default on exec.
        // SAFETY: The structures are initialized and the handler only sets an atomic flag.
        unsafe {
            let mut action: libc::sigaction = mem::zeroed();
            action.sa_sigaction = interrupt as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            let mut previous = mem::MaybeUninit::uninit();
            if libc::sigaction(libc::SIGINT, &action, previous.as_mut_ptr()) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(previous.assume_init()))
        }
    }

    pub(crate) fn interrupted(&self) -> bool {
        INTERRUPTED.load(Ordering::Relaxed)
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        // SAFETY: The saved action came from a successful call to sigaction.
        unsafe { libc::sigaction(libc::SIGINT, &self.0, ptr::null_mut()) };
    }
}

pub(crate) struct Failure {
    step: usize,
    total: usize,
    result: io::Result<ExitStatus>,
}

impl Failure {
    pub fn interrupted(&self) -> bool {
        self.result.as_ref().is_ok_and(|status| {
            status.signal() == Some(libc::SIGINT) || status.code() == Some(128 + libc::SIGINT)
        })
    }

    pub fn terminal_summary(&self) -> String {
        let Self {
            step,
            total,
            result,
        } = self;
        match result {
            Err(error) => format!("Could not run login step {step} of {total}: {error}"),
            Ok(_) if self.interrupted() => format!("Login interrupted at step {step} of {total}"),
            Ok(status) => match status.code() {
                Some(code) => {
                    format!("Login failed at step {step} of {total} (exit code {code}).")
                }
                None => format!("Login failed at step {step} of {total}: {status}"),
            },
        }
    }

    pub fn exit_code(&self) -> ExitCode {
        let Ok(status) = &self.result else {
            return ExitCode::FAILURE;
        };
        if let Some(signal) = status.signal() {
            // SAFETY: The status supplies a valid signal. The caller has dropped its guard.
            unsafe {
                libc::signal(signal, libc::SIG_DFL);
                libc::raise(signal);
            }
            return ExitCode::from((128 + signal) as u8);
        }
        ExitCode::from(status.code().unwrap_or(1) as u8)
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let step = self.step;
        match &self.result {
            Err(error) => write!(f, "Could not run login: step {step}: {error}"),
            Ok(_) if self.interrupted() => write!(f, "Login interrupted at step {step}"),
            Ok(status) => write!(f, "Login failed at step {step}: {status}"),
        }
    }
}

pub(crate) fn run(commands: &[Vec<String>], interrupt: &InterruptGuard) -> Result<(), Failure> {
    for (index, command) in commands.iter().enumerate() {
        let failure = |result| Failure {
            step: index + 1,
            total: commands.len(),
            result,
        };
        let interrupted = || failure(Ok(ExitStatus::from_raw(libc::SIGINT)));
        if interrupt.interrupted() {
            return Err(interrupted());
        }
        let result = status(command);
        if interrupt.interrupted() {
            return Err(interrupted());
        }
        if !result.as_ref().is_ok_and(ExitStatus::success) {
            return Err(failure(result));
        }
    }
    Ok(())
}

fn status(command: &[String]) -> io::Result<ExitStatus> {
    let pid = spawn(command, ptr::null(), ptr::null())?;
    Ok(wait_status(pid, 0)?.expect("blocking wait returns a status"))
}

pub(crate) fn check(command: &[String], cancelled: impl Fn() -> bool) -> io::Result<bool> {
    if cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let pid = spawn_check(command)?;
    let start = Instant::now();
    loop {
        if let Some(status) = wait_status(pid, libc::WNOHANG)? {
            return status
                .code()
                .map(|code| code == 0)
                .ok_or_else(|| io::Error::other(status.to_string()));
        }
        let error = if cancelled() {
            io::ErrorKind::Interrupted.into()
        } else if start.elapsed() >= CHECK_TIMEOUT {
            io::Error::new(io::ErrorKind::TimedOut, "timed out after 5 seconds")
        } else {
            std::thread::sleep(CHECK_INTERVAL);
            continue;
        };
        // SAFETY: The unreaped child owns this PID. Signal its process group first.
        let killed =
            unsafe { libc::kill(-pid, libc::SIGKILL) == 0 || libc::kill(pid, libc::SIGKILL) == 0 };
        wait_status(pid, if killed { 0 } else { libc::WNOHANG })?;
        return Err(error);
    }
}

fn spawn_check(command: &[String]) -> io::Result<libc::pid_t> {
    // SAFETY: Initialize the spawn settings before use and destroy each initialized object.
    unsafe {
        let mut actions = mem::MaybeUninit::uninit();
        spawn_result(libc::posix_spawn_file_actions_init(actions.as_mut_ptr()))?;
        let mut actions = actions.assume_init();
        let result = (|| {
            for fd in 0..=2 {
                spawn_result(libc::posix_spawn_file_actions_addopen(
                    &mut actions,
                    fd,
                    c"/dev/null".as_ptr(),
                    libc::O_RDWR,
                    0,
                ))?;
            }
            let mut attrs = mem::MaybeUninit::uninit();
            spawn_result(libc::posix_spawnattr_init(attrs.as_mut_ptr()))?;
            let mut attrs = attrs.assume_init();
            let result = spawn_result(libc::posix_spawnattr_setpgroup(&mut attrs, 0))
                .and_then(|()| {
                    spawn_result(libc::posix_spawnattr_setflags(
                        &mut attrs,
                        libc::POSIX_SPAWN_SETPGROUP as libc::c_short,
                    ))
                })
                .and_then(|()| spawn(command, &actions, &attrs));
            libc::posix_spawnattr_destroy(&mut attrs);
            result
        })();
        libc::posix_spawn_file_actions_destroy(&mut actions);
        result
    }
}

fn spawn(
    command: &[String],
    actions: *const libc::posix_spawn_file_actions_t,
    attrs: *const libc::posix_spawnattr_t,
) -> io::Result<libc::pid_t> {
    launch(command, |path, argv| {
        let mut pid = 0;
        // SAFETY: The pointers are live and NUL-terminated. SecSetter does not mutate its environment.
        let error = unsafe {
            #[cfg(target_os = "linux")]
            unsafe extern "C" {
                static environ: *mut *mut c_char;
            }
            #[cfg(target_os = "linux")]
            let environment = environ;
            #[cfg(target_os = "macos")]
            let environment = *libc::_NSGetEnviron();
            libc::posix_spawn(
                &mut pid,
                path.as_ptr(),
                actions,
                attrs,
                argv.as_ptr().cast(),
                environment,
            )
        };
        spawn_result(error)?;
        Ok(pid)
    })
}

fn spawn_result(error: libc::c_int) -> io::Result<()> {
    if error == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(error))
    }
}

fn wait_status(pid: libc::pid_t, options: libc::c_int) -> io::Result<Option<ExitStatus>> {
    let mut status = 0;
    loop {
        // SAFETY: pid is the child returned by posix_spawn and status is writable.
        match unsafe { libc::waitpid(pid, &mut status, options) } {
            0 => return Ok(None),
            -1 => {}
            _ => return Ok(Some(ExitStatus::from_raw(status))),
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn launch<T>(
    command: &[String],
    mut start: impl FnMut(&CStr, &[*const c_char]) -> io::Result<T>,
) -> io::Result<T> {
    let arguments = command
        .iter()
        .map(|arg| CString::new(arg.as_str()))
        .collect::<Result<Vec<_>, _>>()?;
    let argv: Vec<_> = arguments
        .iter()
        .map(|arg| arg.as_ptr())
        .chain([ptr::null()])
        .collect();
    let program = &command[0];
    if program.contains('/') {
        return start(&arguments[0], &argv);
    }

    // PATH-searching exec variants can fall back to a shell for an unrecognized executable.
    let path = env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    let mut failure = io::Error::from_raw_os_error(libc::ENOENT);
    for directory in env::split_paths(&path) {
        let candidate = CString::new(directory.join(program).as_os_str().as_bytes())?;
        match start(&candidate, &argv) {
            Err(error) if error.raw_os_error() == Some(libc::EACCES) => failure = error,
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => {}
            result => return result,
        }
    }
    Err(failure)
}
