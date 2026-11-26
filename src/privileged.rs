use std::env;
use std::ffi::{CString, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use rustix::process::{self, Gid, Uid};

use crate::file_write::{FileWrite, Outcome};

pub(crate) const HELPER: &str = "__write";
const COMPLETE: u8 = 1;

pub(crate) fn write(
    operation: &FileWrite,
    destination: &Path,
    (owner, group): (&str, &str),
    bytes: &[u8],
) -> Result<Outcome, String> {
    let channel_error = |error: io::Error| format!("privileged write {destination:?}: {error}");
    // Keep bytes off sudo's stdin, even when sudoers logs redirected input.
    let directory = tempfile::Builder::new()
        .prefix("secsetter-")
        .tempdir_in("/tmp")
        .map_err(channel_error)?;
    let socket_path = directory.path().join("input");
    let listener = UnixListener::bind(&socket_path).map_err(channel_error)?;
    listener.set_nonblocking(true).map_err(channel_error)?;

    let executable = env::current_exe().map_err(channel_error)?;
    let mut command = if process::geteuid().is_root() {
        let mut command = Command::new(executable);
        command.env_remove("SUDO_UID");
        command
    } else {
        let mut command = Command::new("/usr/bin/sudo");
        if File::open("/dev/tty").is_err() {
            command.arg("-n");
        }
        command.args(["-u", "root", "--"]).arg(executable);
        command
    };
    let description = toml::to_string(operation).map_err(|_| "encode privileged operation")?;
    let mut child = command
        .arg(HELPER)
        .arg(socket_path)
        .arg(destination)
        .args([owner, group, &description])
        .stdin(Stdio::null())
        .spawn()
        .map_err(channel_error)?;
    let mut socket = loop {
        match listener.accept() {
            Ok((socket, _)) if peer_uid(&socket).is_ok_and(|uid| uid.is_root()) => break socket,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(channel_error(error));
            }
        }
        if let Some(status) = child.try_wait().map_err(channel_error)? {
            return Err(helper_failure(destination, status));
        }
    };
    drop(listener);
    let reply = exchange(&mut socket, bytes);
    drop(socket);
    let status = child.wait().map_err(channel_error)?;
    if !status.success() {
        return Err(helper_failure(destination, status));
    }
    match reply.map_err(channel_error)?.as_slice() {
        b"0" => Ok(Outcome::Written),
        b"1" => Ok(Outcome::AlreadyExists),
        [b'2', message @ ..] => Err(String::from_utf8_lossy(message).into_owned()),
        _ => Err(format!(
            "privileged write {destination:?}: invalid helper reply"
        )),
    }
}

fn helper_failure(path: &Path, status: ExitStatus) -> String {
    format!("sudo or the privileged helper failed for {path:?}: {status}")
}

fn exchange(socket: &mut UnixStream, bytes: &[u8]) -> io::Result<Vec<u8>> {
    socket.write_all(&(bytes.len() as u64).to_be_bytes())?;
    socket.write_all(bytes)?;
    socket.write_all(&[COMPLETE])?;
    socket.shutdown(Shutdown::Write)?;
    let mut reply = Vec::new();
    socket.read_to_end(&mut reply)?;
    Ok(reply)
}

pub(crate) fn run_helper(args: &[OsString]) -> Result<(), String> {
    if !process::geteuid().is_root() {
        return Err("the privileged write helper requires root".into());
    }
    let [socket, destination, owner, group, description] = args else {
        return Err("invalid privileged write request".into());
    };
    let path = Path::new(destination);
    let mut socket =
        UnixStream::connect(socket).map_err(|error| format!("connect helper: {error}"))?;
    let caller = match env::var("SUDO_UID") {
        Ok(uid) => uid
            .parse()
            .map(Uid::from_raw)
            .map_err(|_| "invalid sudo UID")?,
        Err(env::VarError::NotPresent) => process::geteuid(),
        Err(_) => return Err("invalid sudo UID".into()),
    };
    if peer_uid(&socket).map_err(|error| format!("inspect helper connection: {error}"))? != caller {
        return Err("helper connection is not owned by the invoking user".into());
    }

    let result = (|| {
        let operation: FileWrite = description
            .to_str()
            .and_then(|description| toml::from_str(description).ok())
            .ok_or("invalid privileged write operation")?;
        if !path.is_absolute() {
            return Err("privileged destination must be absolute".into());
        }
        let bytes = read_frame(&mut socket)?;
        if bytes.is_empty() && !matches!(operation, FileWrite::Create) {
            return Err("secret input is empty".into());
        }
        let ownership = lookup_ownership(owner, group)?;
        operation.apply(path, Some(ownership), &bytes)
    })();
    let reply = match result {
        Ok(Outcome::Written) => socket.write_all(b"0"),
        Ok(Outcome::AlreadyExists) => socket.write_all(b"1"),
        Err(error) => socket
            .write_all(b"2")
            .and_then(|()| socket.write_all(error.as_bytes())),
    };
    reply.map_err(|error| format!("send helper result: {error}"))
}

fn read_frame(socket: &mut UnixStream) -> Result<Vec<u8>, String> {
    let read_error = |error| format!("read privileged payload: {error}");
    let mut length = [0; 8];
    socket.read_exact(&mut length).map_err(read_error)?;
    let length = u64::from_be_bytes(length);
    let mut bytes = Vec::new();
    socket
        .take(length)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    let mut marker = [0];
    if bytes.len() as u64 != length || socket.read(&mut marker).map_err(read_error)? == 0 {
        return Err("incomplete privileged payload".into());
    }
    if marker != [COMPLETE] || socket.read(&mut [0]).map_err(read_error)? != 0 {
        return Err("unexpected data after privileged payload".into());
    }
    Ok(bytes)
}

fn lookup_ownership(owner: &OsString, group: &OsString) -> Result<(Uid, Gid), String> {
    let owner_name = CString::new(owner.as_encoded_bytes()).map_err(|_| "invalid owner")?;
    let group_name = CString::new(group.as_encoded_bytes()).map_err(|_| "invalid group")?;
    // SAFETY: Both names are NUL-terminated. The helper is single-threaded and
    // copies each ID immediately from the corresponding account record.
    unsafe {
        let user = libc::getpwnam(owner_name.as_ptr());
        if user.is_null() {
            return Err(format!("unknown owner {owner:?}"));
        }
        let uid = Uid::from_raw((*user).pw_uid);
        let record = libc::getgrnam(group_name.as_ptr());
        if record.is_null() {
            return Err(format!("unknown group {group:?}"));
        }
        Ok((uid, Gid::from_raw((*record).gr_gid)))
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(socket: &UnixStream) -> io::Result<Uid> {
    Ok(rustix::net::sockopt::socket_peercred(socket)?.uid)
}

#[cfg(target_os = "macos")]
fn peer_uid(socket: &UnixStream) -> io::Result<Uid> {
    use std::os::fd::AsRawFd;

    let (mut uid, mut gid) = (0, 0);
    // SAFETY: The socket is live and both output pointers are writable.
    if unsafe { libc::getpeereid(socket.as_raw_fd(), &mut uid, &mut gid) } == 0 {
        Ok(Uid::from_raw(uid))
    } else {
        Err(io::Error::last_os_error())
    }
}
