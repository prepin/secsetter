use std::fs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const ROOT_GROUP: &str = if cfg!(target_os = "macos") {
    "wheel"
} else {
    "root"
};

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
    let write = |operation: &str, length: u64, bytes: &[u8], marker: &[u8]| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_secsetter"))
            .arg("__write")
            .arg(&socket_path)
            .arg(&path)
            .args(["root", ROOT_GROUP, operation])
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
        let incomplete = write("kind = 'replace'", length, secret, b"");
        assert!(String::from_utf8_lossy(&incomplete).contains("incomplete"));
        assert_eq!(fs::read(&path).unwrap(), b"old content");
        let stat = fs::metadata(&path).unwrap();
        assert_eq!((stat.uid(), stat.gid()), (1, 1));
        assert_eq!(fs::read_dir(&base).unwrap().count(), 2);
    }

    fs::write(&path, b"token = 'old content'\n").unwrap();
    let invalid = write(
        "kind = 'field'\nformat = 'toml'\nkey = []",
        3,
        b"new",
        b"\x01",
    );
    assert!(String::from_utf8_lossy(&invalid).contains("key must contain at least one name"));
    assert_eq!(fs::read(&path).unwrap(), b"token = 'old content'\n");
    assert_eq!(fs::read_dir(&base).unwrap().count(), 2);

    assert_eq!(
        write("kind = 'replace'", secret.len() as u64, secret, b"\x01"),
        b"0"
    );
    assert_eq!(fs::read(&path).unwrap(), secret);
    let stat = fs::metadata(path).unwrap();
    assert_eq!((stat.uid(), stat.gid(), stat.mode() & 0o777), (0, 0, 0o600));
}

#[test]
#[ignore = "run as root in an isolated fixture"]
fn public_commands_write_declared_destinations_through_the_root_helper() {
    assert!(rustix::process::geteuid().is_root());
    let ownership = format!("privileged = true\nowner = 'root'\ngroup = '{ROOT_GROUP}'");
    let entry = |id, kind, destination, fields| {
        format!(
            "[[entries]]\nid = '{id}'\nlabel = '{id}'\nkind = '{kind}'\n\
             destination = '$XDG_CONFIG_HOME/{destination}'\n{ownership}\n{fields}\n"
        )
    };
    let config = [
        entry(
            "profile",
            "fnox-file",
            "private/profile",
            "source = 'synthetic profile'",
        ),
        entry(
            "template",
            "template",
            "private/config",
            "source = 'config.toml'",
        ),
        entry("token", "secret-file", "private/token", ""),
        entry(
            "field",
            "secret-field",
            "private/config",
            "format = 'toml'\nkey = ['token']",
        ),
    ]
    .concat();
    let (dir, _) = fixture(&config);
    let base = dir.path().canonicalize().unwrap();
    let templates = base.join("secsetter/templates");
    fs::create_dir(&templates).unwrap();
    fs::write(templates.join("config.toml"), b"token = ''\n# keep\n").unwrap();
    let write_fnox = |body| {
        let fnox = base.join("fnox");
        fs::write(
            &fnox,
            format!(
                "#!/bin/sh\n[ \"$#\" = 3 ] && [ \"$1\" = get ] && [ \"$2\" = --file ] && \
                 [ \"$3\" = 'synthetic profile' ] || exit 45\n\
                 : > \"$XDG_CONFIG_HOME/fetched\"\n{body}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(fnox, fs::Permissions::from_mode(0o700)).unwrap();
    };
    write_fnox(r"printf 'synthetic\000profile\r\n'");

    let run = |args: &[&str], input: Option<&[u8]>| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_secsetter"))
            .env("XDG_CONFIG_HOME", &base)
            .env("PATH", &base)
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(input) = input {
            child.stdin.take().unwrap().write_all(input).unwrap();
        }
        child.wait_with_output().unwrap()
    };
    let success = |output: Output| assert!(output.status.success(), "{output:?}");
    let failure = |output: Output, reason: &str| {
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(reason),
            "{output:?}"
        );
    };
    let private = base.join("private");
    let profile = private.join("profile");
    let token = private.join("token");
    let installed = private.join("config");
    let fetched = base.join("fetched");

    success(run(&["list"], None));
    assert!(!fetched.exists());
    failure(run(&["set", "token"], Some(b"input")), "does not exist");
    assert!(!private.exists());

    fs::create_dir(&private).unwrap();
    success(run(&["install", "template"], None));
    assert_root_file(&installed, b"token = ''\n# keep\n");
    success(run(&["set", "field"], Some(b"new\n")));
    let edited = fs::read(&installed).unwrap();
    let document: toml::Value = toml::from_str(str::from_utf8(&edited).unwrap()).unwrap();
    assert_eq!(document["token"].as_str(), Some("new\n"));
    assert!(edited.ends_with(b"# keep\n"));
    success(run(&["install", "template"], None));
    assert_root_file(&installed, &edited);

    success(run(&["set", "token"], Some(b"first")));
    success(run(&["set", "token"], Some(b"replacement\0\r\n")));
    assert_root_file(&token, b"replacement\0\r\n");

    success(run(&["restore", "profile"], None));
    assert_root_file(&profile, b"synthetic\0profile\r\n");
    fs::remove_file(&fetched).unwrap();
    failure(run(&["restore", "profile"], None), "--replace");
    assert!(!fetched.exists());
    write_fnox("printf 'synthetic-failed-fetch' >&2; exit 42");
    let output = run(&["restore", "profile", "--replace"], None);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-failed-fetch"));
    failure(output, "42");
    assert_root_file(&profile, b"synthetic\0profile\r\n");
    write_fnox(r"printf 'updated\r\n'");
    success(run(&["restore", "profile", "--replace"], None));
    assert_root_file(&profile, b"updated\r\n");

    let moved = base.join("moved");
    fs::rename(&private, &moved).unwrap();
    symlink(&moved, &private).unwrap();
    failure(run(&["set", "token"], Some(b"refused")), "symbolic link");
    assert!(private.is_symlink());
    assert_root_file(&moved.join("token"), b"replacement\0\r\n");
    assert_eq!(fs::read_dir(&moved).unwrap().count(), 3);

    failure(run(&["clear", "token"], None), "symbolic link");
    fs::remove_file(&private).unwrap();
    fs::rename(&moved, &private).unwrap();
    fs::remove_file(&fetched).unwrap();
    success(run(&["clear", "profile"], None));
    success(run(&["clear", "token"], None));
    success(run(&["clear", "field"], None));
    assert!(!profile.exists() && !token.exists() && !fetched.exists());
    assert_root_file(&installed, b"token = \"\"\n# keep\n");
}

fn assert_root_file(path: &Path, bytes: &[u8]) {
    assert_eq!(fs::read(path).unwrap(), bytes);
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
