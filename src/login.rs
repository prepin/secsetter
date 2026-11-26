use std::ffi::{CStr, CString, c_char};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::{env, io, ptr};

pub(crate) fn exec(command: &[String]) -> io::Error {
    launch::<()>(command, |path, argv| {
        // SAFETY: Both arguments are live and NUL-terminated. Exec only returns on failure.
        unsafe { libc::execv(path.as_ptr(), argv.as_ptr()) };
        Err(io::Error::last_os_error())
    })
    .unwrap_err()
}

pub(crate) fn status(command: &[String]) -> io::Result<ExitStatus> {
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
