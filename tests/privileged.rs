use std::fs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::process::Command;

fn fixture(entry: &str) -> (tempfile::TempDir, Command) {
    let dir = tempfile::tempdir().unwrap();
    let config_home = dir.path().canonicalize().unwrap();
    fs::create_dir(config_home.join("secsetter")).unwrap();
    fs::write(config_home.join("secsetter/config.toml"), entry).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_secsetter"));
    command.env("XDG_CONFIG_HOME", config_home);
    (dir, command)
}

#[test]
#[ignore = "run in an isolated root fixture with UID and GID 1 mapped"]
fn direct_helper_commits_only_complete_frames_and_sets_declared_ownership() {
    assert_eq!(rustix::process::geteuid().as_raw(), 0);
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let base = dir.path().canonicalize().unwrap();
    let path = base.join("config");
    fs::write(&path, b"old content").unwrap();
    rustix::fs::chown(
        &path,
        Some(rustix::process::Uid::from_raw(1)),
        Some(rustix::process::Gid::from_raw(1)),
    )
    .unwrap();
    let socket_path = base.join("s");
    let listener = UnixListener::bind(&socket_path).unwrap();
    let group = if cfg!(target_os = "macos") {
        "wheel"
    } else {
        "root"
    };
    let write = |length: u64, bytes: &[u8], marker: &[u8]| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_secsetter"))
            .arg("__write")
            .arg(&socket_path)
            .arg(&path)
            .args(["root", group, "kind = 'replace'"])
            .env_remove("SUDO_UID")
            .spawn()
            .unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        child.try_wait().unwrap().is_none(),
                        "helper did not connect"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        };
        socket.write_all(&length.to_be_bytes()).unwrap();
        socket.write_all(bytes).unwrap();
        socket.write_all(marker).unwrap();
        socket.shutdown(Shutdown::Write).unwrap();
        let mut reply = Vec::new();
        socket.read_to_end(&mut reply).unwrap();
        assert!(child.wait().unwrap().success(), "{reply:?}");
        reply
    };

    let secret = b"synthetic\x00payload\r\n";
    for length in [secret.len() as u64 + 1, secret.len() as u64] {
        let incomplete = write(length, secret, b"");
        assert!(String::from_utf8_lossy(&incomplete).contains("incomplete"));
        assert_eq!(fs::read(&path).unwrap(), b"old content");
        let stat = fs::metadata(&path).unwrap();
        assert_eq!((stat.uid(), stat.gid()), (1, 1));
        assert_eq!(fs::read_dir(&base).unwrap().count(), 2);
    }

    assert_eq!(write(secret.len() as u64, secret, b"\x01"), b"0");
    assert_eq!(fs::read(&path).unwrap(), secret);
    let stat = fs::metadata(path).unwrap();
    assert_eq!((stat.uid(), stat.gid(), stat.mode() & 0o777), (0, 0, 0o600));
}

#[test]
fn list_accepts_declared_ownership_and_rejects_incomplete_ownership() {
    let entry = r#"
[[entries]]
id = "demo"
label = "Synthetic secret"
kind = "secret-file"
destination = "$XDG_CONFIG_HOME/secret"
privileged = true
"#;
    for (ownership, missing) in [
        ("owner = 'root'\ngroup = 'root'", None),
        ("owner = 'root'", Some("group")),
        ("group = 'root'", Some("owner")),
    ] {
        let (_dir, mut command) = fixture(&format!("{entry}{ownership}"));
        let output = command.arg("list").output().unwrap();
        if let Some(missing) = missing {
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains(missing));
        } else {
            assert!(output.status.success(), "{output:?}");
            assert_eq!(output.stdout, b"demo\tSynthetic secret\tNot set\n");
        }
    }
}
