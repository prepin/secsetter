use std::collections::HashSet;
use std::env;
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{self, Mode, OFlags};
use serde::Deserialize;

use crate::login::JsonCheck;
use crate::secret_field::{Format, validate_key};

pub(crate) struct Config {
    pub directory: OwnedFd,
    pub path: PathBuf,
    pub entries: Vec<Entry>,
}

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    entries: Vec<Entry>,
}

#[derive(Deserialize)]
pub(crate) struct Entry {
    pub id: String,
    pub label: String,
    pub section: Option<String>,
    pub url: Option<String>,
    platforms: Option<Vec<Platform>>,
    #[serde(default)]
    privileged: bool,
    owner: Option<String>,
    group: Option<String>,
    #[serde(flatten)]
    pub kind: Kind,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum Kind {
    Template {
        source: PathBuf,
        destination: PathBuf,
    },
    SecretFile {
        destination: PathBuf,
    },
    SecretField {
        destination: PathBuf,
        format: Format,
        key: Vec<String>,
    },
    FnoxFile {
        source: String,
        destination: PathBuf,
    },
    Login {
        commands: Vec<Vec<String>>,
        #[serde(default)]
        ready_when: Vec<LoginCheck>,
    },
}

#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum LoginCheck {
    FileExists {
        path: PathBuf,
    },
    FieldSet {
        path: PathBuf,
        format: Format,
        key: Vec<String>,
    },
    Command {
        command: Vec<String>,
        json: Option<JsonCheck>,
    },
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Platform {
    Linux,
    Macos,
}

impl Entry {
    pub fn ownership(&self) -> Option<(&str, &str)> {
        self.privileged
            .then(|| self.owner.as_deref().zip(self.group.as_deref()).unwrap())
    }

    pub fn applies_here(&self) -> bool {
        let current = if cfg!(target_os = "linux") {
            Platform::Linux
        } else {
            Platform::Macos
        };
        self.platforms
            .as_ref()
            .is_none_or(|platforms| platforms.contains(&current))
    }

    pub fn command_readiness(&self) -> Option<&[LoginCheck]> {
        match &self.kind {
            Kind::Login { ready_when, .. }
                if ready_when
                    .iter()
                    .any(|check| matches!(check, LoginCheck::Command { .. })) =>
            {
                Some(ready_when)
            }
            _ => None,
        }
    }
}

impl Config {
    pub fn load(config_path: Option<&Path>) -> Result<Self, String> {
        let config_path = match config_path {
            Some(path) => path.to_owned(),
            None => config_home()?.join("secsetter/config.toml"),
        };
        let read_error = |error: &dyn std::fmt::Display| format!("read {config_path:?}: {error}");
        let path = config_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        // Keep a trailing slash from turning a directory path into a file name.
        let name = config_path
            .as_os_str()
            .as_encoded_bytes()
            .rsplit(|&byte| byte == b'/')
            .next()
            .unwrap();
        let directory = fs::open(path, OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
            .map_err(|error| read_error(&error))?;
        let mut file = File::from(
            fs::openat(
                &directory,
                name,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| read_error(&error))?,
        );
        if !file
            .metadata()
            .map_err(|error| read_error(&error))?
            .is_file()
        {
            return Err(format!("read {config_path:?}: expected a regular file"));
        }
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|error| read_error(&error))?;
        let ConfigFile { mut entries } = toml::from_str(&text)
            .map_err(|error| config_error(&config_path, &text, error.span()))?;

        let mut ids = HashSet::new();
        for entry in &mut entries {
            let id = &entry.id;
            let invalid =
                |reason: &str| format!("invalid config {config_path:?}, entry {id:?}: {reason}");
            if id.is_empty() {
                return Err(invalid("id must not be empty"));
            }
            if !ids.insert(id) {
                return Err(invalid("duplicate id"));
            }
            if entry.section.as_deref().is_some_and(|section| {
                section.trim().is_empty() || section.chars().any(char::is_control)
            }) {
                return Err(invalid(
                    "section must be non-blank and contain no control characters",
                ));
            }
            if let Some(url) = &entry.url {
                if !matches!(
                    entry.kind,
                    Kind::SecretFile { .. } | Kind::SecretField { .. }
                ) {
                    return Err(invalid(
                        "url is only supported for secret-file and secret-field",
                    ));
                }
                if !valid_token_url(url) {
                    return Err(invalid(
                        "url must be an absolute HTTP or HTTPS URL without credentials",
                    ));
                }
            }
            if let Kind::SecretField { key, .. } = &entry.kind {
                validate_key(key).map_err(invalid)?;
            }
            let destination = match &mut entry.kind {
                Kind::Template {
                    source,
                    destination,
                } => {
                    if source.is_absolute()
                        || source.file_name().is_none()
                        || source.as_os_str().as_encoded_bytes().contains(&0)
                        || source.components().any(|part| part == Component::ParentDir)
                    {
                        return Err(invalid("source must be a file path inside templates/"));
                    }
                    destination
                }
                Kind::SecretFile { destination } | Kind::SecretField { destination, .. } => {
                    destination
                }
                Kind::FnoxFile {
                    source,
                    destination,
                } => {
                    if source.is_empty() || source.contains('\0') {
                        return Err(invalid("fnox source must be non-empty and contain no NUL"));
                    }
                    destination
                }
                Kind::Login {
                    commands,
                    ready_when,
                } => {
                    if entry.privileged {
                        return Err(invalid("login entries cannot be privileged"));
                    }
                    if commands.is_empty() {
                        return Err(invalid("commands must contain at least one step"));
                    }
                    for (index, command) in commands.iter().enumerate() {
                        validate_command(command)
                            .map_err(|reason| invalid(&format!("step {} {reason}", index + 1)))?;
                    }
                    for (index, check) in ready_when.iter_mut().enumerate() {
                        let invalid_check = |reason: &str| {
                            invalid(&format!("ready_when check {}: {reason}", index + 1))
                        };
                        let path = match check {
                            LoginCheck::FileExists { path } => path,
                            LoginCheck::FieldSet { path, key, .. } => {
                                validate_key(key).map_err(invalid_check)?;
                                path
                            }
                            LoginCheck::Command { command, json } => {
                                validate_command(command).map_err(|reason| {
                                    invalid_check(&format!("command {reason}"))
                                })?;
                                if let Some(json) = json {
                                    json.validate().map_err(invalid_check)?;
                                }
                                continue;
                            }
                        };
                        *path = expand_path(path).map_err(|reason| invalid_check(&reason))?;
                    }
                    continue;
                }
            };
            if entry.privileged {
                for (name, value) in [("owner", &entry.owner), ("group", &entry.group)] {
                    if value
                        .as_ref()
                        .is_none_or(|value| value.is_empty() || value.contains('\0'))
                    {
                        return Err(invalid(&format!(
                            "privileged entries require a valid {name}"
                        )));
                    }
                }
            }
            *destination = expand_path(destination).map_err(|reason| invalid(&reason))?;
        }
        Ok(Self {
            directory,
            path: path.to_owned(),
            entries,
        })
    }
}

fn validate_command(command: &[String]) -> Result<(), &'static str> {
    if command.first().is_none_or(String::is_empty) {
        return Err("must start with an executable");
    }
    if command.iter().any(|argument| argument.contains('\0')) {
        return Err("must not contain NUL characters");
    }
    Ok(())
}

fn valid_token_url(text: &str) -> bool {
    text.split_once("://").is_some_and(|(scheme, rest)| {
        (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
            && !rest.is_empty()
            && !rest.starts_with('/')
            && !text.contains('\\')
            && !text.chars().any(|c| c.is_control() || c.is_whitespace())
    }) && url::Url::parse(text)
        .is_ok_and(|url| url.has_host() && url.username().is_empty() && url.password().is_none())
}

fn config_home() -> Result<PathBuf, String> {
    match env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(value) if Path::new(&value).is_absolute() => Ok(PathBuf::from(value)),
        Some(_) => Err("XDG_CONFIG_HOME must be an absolute path".into()),
        None => Ok(home()?.join(".config")),
    }
}

fn home() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| "HOME must be an absolute path".into())
}

fn expand_path(path: &Path) -> Result<PathBuf, String> {
    let value = path.to_str().expect("TOML paths are UTF-8");
    let expanded = if let Some(rest) = value.strip_prefix("~/") {
        home()?.join(rest.trim_start_matches('/'))
    } else if let Some(rest) = value.strip_prefix("$XDG_CONFIG_HOME/") {
        config_home()?.join(rest.trim_start_matches('/'))
    } else {
        path.to_owned()
    };
    if !expanded.is_absolute() || expanded.as_os_str().as_encoded_bytes().contains(&0) {
        return Err("path must be an absolute path or start with ~/ or $XDG_CONFIG_HOME/".into());
    }
    Ok(expanded)
}

fn config_error(path: &Path, text: &str, span: Option<std::ops::Range<usize>>) -> String {
    let location = span.map_or_else(String::new, |span| {
        let line = text.as_bytes()[..span.start]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count()
            + 1;
        format!(" at line {line}")
    });
    format!("invalid TOML or entry fields in {path:?}{location}")
}
