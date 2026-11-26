use std::ffi::{CStr, CString, c_char};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitCode, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{env, io, mem, ptr};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

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

    fn interrupted(&self) -> bool {
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
    let pid = launch(command, |path, argv| {
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
                ptr::null(),
                ptr::null(),
                argv.as_ptr().cast(),
                environment,
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        Ok(pid)
    })?;

    let mut status = 0;
    loop {
        // SAFETY: pid is the child returned by posix_spawn and status is writable.
        if unsafe { libc::waitpid(pid, &mut status, 0) } != -1 {
            return Ok(ExitStatus::from_raw(status));
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
