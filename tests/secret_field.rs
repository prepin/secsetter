use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

use rustix::fs::{FlockOperation, flock};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    destination: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::with_format("yaml", "yml")
    }

    fn with_format(format: &str, extension: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let destination = format!("glab/config.{extension}");
        fs::create_dir(root.join("secsetter")).unwrap();
        fs::write(
            root.join("secsetter/config.toml"),
            format!(
                r#"
[[entries]]
id = "token"
label = "GitLab token"
kind = "secret-field"
destination = "$XDG_CONFIG_HOME/{destination}"
format = "{format}"
key = ["hosts", "gitlab.example.com", "token"]
"#,
            ),
        )
        .unwrap();
        Self {
            _dir: dir,
            destination: root.join(destination),
            root,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_secsetter"));
        command.env("XDG_CONFIG_HOME", &self.root);
        command
    }

    fn destination(&self) -> PathBuf {
        self.destination.clone()
    }

    fn start_set(&self, id: &str) -> Child {
        self.command()
            .args(["set", id])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn set(&self, input: &[u8]) -> Output {
        let mut child = self.start_set("token");
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    fn list(&self) -> String {
        success(self.command().arg("list").output().unwrap())
    }
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout).unwrap()
}

fn failure(output: Output, reason: &str) -> String {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains(reason), "{error}");
    assert!(!error.contains("secret-value"), "{error}");
    error
}

#[test]
fn replaces_a_literal_toml_field_and_keeps_comments_and_other_values() {
    let fixture = Fixture::with_format("toml", "toml");
    let path = fixture.destination();
    fs::create_dir(path.parent().unwrap()).unwrap();
    let original = r#"# hosts file
hosts."gitlab\u002Eexample.com".token = '' # keep token comment
hosts."gitlab\u002Eexample.com".retries = 0x10 # keep original integer
hosts.gitlab.example.com.token = 'unrelated'
# footer
"#;
    fs::write(&path, original).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let old_metadata = fs::metadata(&path).unwrap();

    assert_eq!(fixture.list(), "token\tGitLab token\tNot set\n");
    assert_eq!(success(fixture.set(b"synthetic-token")), "Set\n");
    assert_eq!(fixture.list(), "token\tGitLab token\tSet\n");
    let expected = original.replacen("''", "\"synthetic-token\"", 1);
    assert_eq!(fs::read_to_string(&path).unwrap(), expected);

    let secret = "  \"quoted\"\\ café\r\n\0\u{1b}\n \n";
    assert_eq!(success(fixture.set(secret.as_bytes())), "Set\n");
    let edited = fs::read_to_string(&path).unwrap();
    let document: toml::Table = toml::from_str(&edited).unwrap();
    assert_eq!(
        document["hosts"]["gitlab.example.com"]["token"].as_str(),
        Some(secret)
    );
    let new_metadata = fs::metadata(&path).unwrap();
    assert_eq!(new_metadata.mode() & 0o777, 0o600);
    assert_eq!(new_metadata.uid(), old_metadata.uid());
    assert_ne!(new_metadata.ino(), old_metadata.ino());
}

#[test]
fn adds_a_toml_field_to_an_existing_inline_table() {
    let fixture = Fixture::with_format("toml", "toml");
    let path = fixture.destination();
    fs::create_dir(path.parent().unwrap()).unwrap();
    let original = r#"hosts = { "gitlab.example.com".retries = 0x10, other = {} } # hosts
# footer
"#;
    fs::write(&path, original).unwrap();
    assert_eq!(fixture.list(), "token\tGitLab token\tNot set\n");

    assert_eq!(success(fixture.set(b"synthetic-token")), "Set\n");
    assert_eq!(fixture.list(), "token\tGitLab token\tSet\n");
    let edited = fs::read_to_string(&path).unwrap();
    let document: toml::Table = toml::from_str(&edited).unwrap();
    assert_eq!(
        document["hosts"]["gitlab.example.com"]["token"].as_str(),
        Some("synthetic-token")
    );
    assert!(edited.contains(r#""gitlab.example.com".retries = 0x10"#));
    assert!(edited.ends_with("} # hosts\n# footer\n"));
    assert!(document["hosts"]["other"].as_table().unwrap().is_empty());
}

#[test]
fn sets_a_literal_json_field_without_changing_unrelated_numbers_or_values() {
    let fixture = Fixture::with_format("json", "json");
    let path = fixture.destination();
    fs::create_dir(path.parent().unwrap()).unwrap();
    let original = r#"{
  "hosts": {
    "gitlab\u002eexample.com": {
      "options": [0.12345678901234567890123456789, 18446744073709551616, 1e999, -0],
      "token": ""
    },
    "gitlab": {"example": {"com": {"token": "unrelated"}}}
  }
}
"#;
    fs::write(&path, original).unwrap();
    assert_eq!(fixture.list(), "token\tGitLab token\tNot set\n");

    let secret = " \t\"quoted\"\\ café\r\n\0\u{1b}\n \n";
    let literal = r#"" \t\"quoted\"\\ café\r\n\u0000\u001b\n \n""#;
    assert_eq!(success(fixture.set(secret.as_bytes())), "Set\n");
    assert_eq!(fixture.list(), "token\tGitLab token\tSet\n");
    let expected = original.replacen("\"token\": \"\"", &format!("\"token\": {literal}"), 1);
    assert_eq!(fs::read_to_string(&path).unwrap(), expected);
}

#[test]
fn adds_a_json_field_to_empty_and_nonempty_objects() {
    let fixture = Fixture::with_format("json", "json");
    let path = fixture.destination();
    fs::create_dir(path.parent().unwrap()).unwrap();

    for (host, expected) in [
        ("{\n    }", "{\"token\":\"synthetic-token\"\n    }"),
        (
            "{\"retries\":2\n    }",
            "{\"retries\":2,\"token\":\"synthetic-token\"\n    }",
        ),
    ] {
        let original = format!(r#"{{"hosts":{{"gitlab.example.com":{host}}}}}"#);
        fs::write(&path, &original).unwrap();
        assert_eq!(fixture.list(), "token\tGitLab token\tNot set\n");
        assert_eq!(success(fixture.set(b"synthetic-token")), "Set\n");
        assert_eq!(fixture.list(), "token\tGitLab token\tSet\n");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original.replacen(host, expected, 1)
        );
    }
}

#[test]
fn blocks_invalid_toml_and_json_documents_and_paths_without_excerpts() {
    let cases: &[(&str, &str, &str)] = &[
        ("toml", "hosts = 'old-secret-value", "invalid TOML document"),
        (
            "toml",
            r#"[hosts."gitlab.example.com"]
token = "old-secret-value"
"to\u006ben" = ""
"#,
            "duplicate key",
        ),
        (
            "toml",
            r#"[[hosts."gitlab.example.com"]]
token = "old-secret-value"
"#,
            "expected a TOML table",
        ),
        (
            "toml",
            r#"hosts."gitlab.example.com".token = ["old-secret-value"]"#,
            "expected a TOML string",
        ),
        (
            "json",
            r#"{"hosts":{} /* old-secret-value */}"#,
            "invalid JSON document",
        ),
        (
            "json",
            r#"{"hosts":{"gitlab.example.com":{"token":"old-secret-value","to\u006ben":""}}}"#,
            "duplicate key",
        ),
        (
            "json",
            r#"{"hosts":{"gitlab.example.com":{}},"unrelated":[{"x":"old-secret-value","x":1}]}"#,
            "duplicate key",
        ),
        (
            "json",
            r#"{"hosts":[{"gitlab.example.com":{"token":"old-secret-value"}}]}"#,
            "expected a JSON object",
        ),
        (
            "json",
            r#"{"hosts":{"gitlab.example.com":{"token":null}}}"#,
            "expected a JSON string",
        ),
        ("json", "null", "expected a JSON object"),
    ];
    for (format, original, reason) in cases {
        let fixture = Fixture::with_format(format, format);
        let path = fixture.destination();
        fs::create_dir(path.parent().unwrap()).unwrap();
        fs::write(&path, original).unwrap();
        let listed = fixture.list();
        assert!(listed.contains("\tBlocked:"), "{listed}");
        assert!(listed.contains(reason), "{listed}");
        assert!(!listed.contains("secret-value"), "{listed}");
        let error = failure(fixture.set(b"incoming-secret-value"), reason);
        assert!(error.contains(&format!("glab/config.{format}")), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), *original);
    }
}

#[test]
fn reports_missing_toml_and_json_prerequisites_without_creating_them() {
    for (format, original) in [
        ("toml", "[hosts]\nother = 3\n"),
        ("json", r#"{"hosts":{"other":3}}"#),
    ] {
        let fixture = Fixture::with_format(format, format);
        let path = fixture.destination();
        assert_eq!(fixture.list(), "token\tGitLab token\tNot set\n");
        failure(fixture.set(b"synthetic-token"), "requires an existing file");
        assert!(!path.exists());
        fs::create_dir(path.parent().unwrap()).unwrap();
        fs::write(&path, original).unwrap();
        assert_eq!(fixture.list(), "token\tGitLab token\tNot set\n");
        failure(
            fixture.set(b"synthetic-token"),
            "parent object [\"hosts\", \"gitlab.example.com\"] does not exist",
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }
}

#[test]
fn replaces_a_literal_yaml_field_and_keeps_comments_and_other_values() {
    let fixture = Fixture::new();
    let path = fixture.destination();
    fs::create_dir(path.parent().unwrap()).unwrap();
    let original = "\
# hosts file
hosts:
  'gitlab.example.com': # literal host key
    token: \"\" # keep token comment
    retries: 0x10 # keep the original integer
  gitlab:
    example:
      com: {token: 'unrelated'}
# footer
";
    fs::write(&path, original).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let old_metadata = fs::metadata(&path).unwrap();

    assert_eq!(fixture.list(), "token\tGitLab token\tNot set\n");
    assert_eq!(success(fixture.set(b"synthetic-token")), "Set\n");
    assert_eq!(fixture.list(), "token\tGitLab token\tSet\n");
    let edited = fs::read_to_string(&path).unwrap();
    let token_line = edited
        .lines()
        .find(|line| line.starts_with("    token: "))
        .unwrap();
    let (token, _) = token_line.split_once(" # keep token comment").unwrap();
    assert!(token.contains("synthetic-token"));
    assert_eq!(edited.replacen(token, "    token: \"\"", 1), original);
    let new_metadata = fs::metadata(&path).unwrap();
    assert_eq!(new_metadata.mode() & 0o777, 0o600);
    assert_eq!(new_metadata.uid(), old_metadata.uid());
    assert_ne!(new_metadata.ino(), old_metadata.ino());
}

#[test]
fn adds_a_missing_field_and_preserves_the_flow_mapping_and_exact_input() {
    let fixture = Fixture::new();
    let path = fixture.destination();
    fs::create_dir(path.parent().unwrap()).unwrap();
    let original = "\
---\r\n\
hosts: {\"gitlab\\u002Eexample.com\": {options: [null, 0x10, 'a: #'],}, other: {}} # tail\r\n\
... # end marker\r\n";
    fs::write(&path, original).unwrap();
    assert_eq!(fixture.list(), "token\tGitLab token\tNot set\n");

    let secret = "  null: # brackets [x]\r\ncafé\0\u{1b}\u{85}\u{2028}\u{2029}\n \n";
    assert_eq!(success(fixture.set(secret.as_bytes())), "Set\n");
    let edited = fs::read_to_string(&path).unwrap();
    let document = oneyaml::Document::parse(edited.as_str().to_owned()).unwrap();
    let field = document
        .get("hosts")
        .unwrap()
        .get("gitlab.example.com")
        .unwrap()
        .get("token")
        .unwrap();
    assert_eq!(field.as_str().as_deref(), Some(secret));
    assert!(edited.starts_with(
        "---\r\nhosts: {\"gitlab\\u002Eexample.com\": {options: [null, 0x10, 'a: #'],"
    ));
    assert!(edited.ends_with("}, other: {}} # tail\r\n... # end marker\r\n"));
}

#[test]
fn replaces_a_block_scalar_without_losing_its_comment_or_following_values() {
    let fixture = Fixture::new();
    let path = fixture.destination();
    fs::create_dir(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "\
hosts:
  gitlab.example.com:
    token: |+ # block header comment
      old-secret-value

    # status comment
    status: yes
",
    )
    .unwrap();

    assert_eq!(success(fixture.set(b"null")), "Set\n");
    let edited = fs::read_to_string(&path).unwrap();
    assert!(edited.contains("# block header comment\n"), "{edited}");
    assert!(
        edited.ends_with("    # status comment\n    status: yes\n"),
        "{edited}"
    );
    assert!(!edited.contains("old-secret-value"));
    let document = oneyaml::Document::parse(edited).unwrap();
    assert_eq!(
        document
            .get("hosts")
            .unwrap()
            .get("gitlab.example.com")
            .unwrap()
            .get("token")
            .unwrap()
            .as_str()
            .as_deref(),
        Some("null")
    );
}

#[test]
fn reports_missing_prerequisites_without_creating_objects_or_files() {
    let fixture = Fixture::new();
    let path = fixture.destination();
    assert_eq!(fixture.list(), "token\tGitLab token\tNot set\n");
    let error = failure(
        fixture.set(b"incoming-secret-value"),
        "requires an existing file",
    );
    assert!(error.contains("glab/config.yml"), "{error}");
    assert!(!path.parent().unwrap().exists());
    fs::create_dir(path.parent().unwrap()).unwrap();
    failure(
        fixture.set(b"incoming-secret-value"),
        "requires an existing file",
    );
    assert!(!path.exists());

    for (original, reason) in [
        ("# no root object\n", "root object does not exist"),
        ("---\n# no root object\n", "root object does not exist"),
        ("{}\n", "parent object [\"hosts\"] does not exist"),
        (
            "hosts: {}\n",
            "parent object [\"hosts\", \"gitlab.example.com\"] does not exist",
        ),
    ] {
        fs::write(&path, original).unwrap();
        assert_eq!(fixture.list(), "token\tGitLab token\tNot set\n");
        failure(fixture.set(b"incoming-secret-value"), reason);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }
}

#[test]
fn blocks_non_strings_invalid_documents_and_ambiguous_yaml_paths_without_excerpts() {
    let fixture = Fixture::new();
    let path = fixture.destination();
    fs::create_dir(path.parent().unwrap()).unwrap();
    let cases: &[(&[u8], &str)] = &[
        (b"null\n", "expected a YAML object"),
        (b"hosts: null\n", "expected a YAML object"),
        (b"hosts: [gitlab.example.com]\n", "expected a YAML object"),
        (
            b"hosts: {gitlab.example.com: {token: null}}\n",
            "expected a YAML string",
        ),
        (
            b"hosts: {gitlab.example.com: {token:}}\n",
            "expected a YAML string",
        ),
        (
            b"hosts: {gitlab.example.com: {token: 12}}\n",
            "expected a YAML string",
        ),
        (
            b"hosts: {gitlab.example.com: {token: false}}\n",
            "expected a YAML string",
        ),
        (
            b"hosts: {gitlab.example.com: {token: [secret-value]}}\n",
            "expected a YAML string",
        ),
        (
            b"hosts: {gitlab.example.com: {token: 'old-secret-value,}}\n",
            "invalid YAML document",
        ),
        (
            b"hosts: {gitlab.example.com: {token: old-secret-value, \"to\\u006Ben\": ''}}\n",
            "duplicate key",
        ),
        (
            b"hosts: {gitlab.example.com: {token: old-secret-value}}\n---\n{}\n",
            "expected a single document",
        ),
        (
            b"actual: &actual {token: old-secret-value}\nhosts: {gitlab.example.com: *actual}\n",
            "value is an alias",
        ),
        (
            b"hosts: {gitlab.example.com: {token: &actual old-secret-value}}\nother: *actual\n",
            "value is used by an alias",
        ),
        (
            b"hosts: {gitlab.example.com: &actual {}}\nother: *actual\n",
            "value is used by an alias",
        ),
        (
            b"name: &name token\nhosts: {gitlab.example.com: {*name : old-secret-value}}\n",
            "key is an alias",
        ),
        (
            b"base: &base {token: old-secret-value}\nhosts: {gitlab.example.com: {<<: *base}}\n",
            "key comes from a merge",
        ),
        (
            b"other: old-secret-value\xff\n",
            "document is not valid UTF-8",
        ),
    ];
    for (original, reason) in cases {
        fs::write(&path, original).unwrap();
        let listed = fixture.list();
        assert!(listed.contains("\tBlocked:"), "{listed}");
        assert!(listed.contains(reason), "{listed}");
        assert!(!listed.contains("secret-value"), "{listed}");
        let error = failure(fixture.set(b"incoming-secret-value"), reason);
        assert!(error.contains("glab/config.yml"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), *original);
    }
}

#[test]
fn rejects_invalid_utf8_input_and_does_not_follow_links() {
    let fixture = Fixture::new();
    let path = fixture.destination();
    fs::create_dir(path.parent().unwrap()).unwrap();
    let original = b"hosts: {gitlab.example.com: {token: old-secret-value}}\n";
    fs::write(&path, original).unwrap();
    assert_eq!(fixture.list(), "token\tGitLab token\tSet\n");

    failure(
        fixture.set(b"incoming-secret-value\xff"),
        "secret input must be valid UTF-8",
    );
    failure(fixture.set(b""), "secret input is empty");
    assert_eq!(fs::read(&path).unwrap(), original);

    let outside = fixture.root.join("outside.yml");
    fs::rename(&path, &outside).unwrap();
    symlink(&outside, &path).unwrap();
    assert!(fixture.list().contains("Blocked:"));
    failure(fixture.set(b"incoming-secret-value"), "symbolic link");
    assert!(path.is_symlink());
    assert_eq!(fs::read(outside).unwrap(), original);
}

#[test]
fn concurrent_set_commands_keep_both_fields() {
    let fixture = Fixture::new();
    let path = fixture.destination();
    let parent = path.parent().unwrap();
    fs::create_dir(parent).unwrap();
    let initial = "hosts: {gitlab.example.com: {}} # initial\n";
    fs::write(&path, initial).unwrap();
    writeln!(
        OpenOptions::new()
            .append(true)
            .open(fixture.root.join("secsetter/config.toml"))
            .unwrap(),
        r#"
[[entries]]
id = "second"
label = "Second token"
kind = "secret-field"
destination = "$XDG_CONFIG_HOME/glab/config.yml"
format = "yaml"
key = ["hosts", "gitlab.example.com", "second"]
"#
    )
    .unwrap();

    let guard = File::open(parent).unwrap();
    flock(&guard, FlockOperation::LockExclusive).unwrap();
    let mut first = fixture.start_set("token");
    let mut second = fixture.start_set("second");
    first
        .stdin
        .take()
        .unwrap()
        .write_all(b"first-token")
        .unwrap();
    second
        .stdin
        .take()
        .unwrap()
        .write_all(b"second-token")
        .unwrap();
    assert!(first.try_wait().unwrap().is_none());
    assert!(second.try_wait().unwrap().is_none());
    fs::write(&path, initial.replace("initial", "latest")).unwrap();
    drop(guard);
    assert_eq!(success(first.wait_with_output().unwrap()), "Set\n");
    assert_eq!(success(second.wait_with_output().unwrap()), "Set\n");

    let edited = fs::read_to_string(&path).unwrap();
    let document = oneyaml::Document::parse(edited.as_str().to_owned()).unwrap();
    let host = document
        .get("hosts")
        .unwrap()
        .get("gitlab.example.com")
        .unwrap();
    assert_eq!(
        host.get("token").unwrap().as_str().as_deref(),
        Some("first-token")
    );
    assert_eq!(
        host.get("second").unwrap().as_str().as_deref(),
        Some("second-token")
    );
    assert!(edited.ends_with("# latest\n"), "{edited}");
}
