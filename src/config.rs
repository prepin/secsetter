use std::collections::HashSet;
use std::env;
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{self, Mode, OFlags};
use serde::Deserialize;

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
    platforms: Option<Vec<Platform>>,
    #[serde(default)]
    privileged: bool,
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
    Login {
        command: Vec<String>,
    },
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Platform {
    Linux,
    Macos,
}

impl Entry {
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
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let config_home = match env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
            Some(value) => PathBuf::from(value),
            None => home()?.join(".config"),
        };
        if !config_home.is_absolute() {
            return Err("XDG_CONFIG_HOME must be an absolute path".into());
        }
        let path = config_home.join("secsetter");
        let config_path = path.join("config.toml");
        let read_error = |error: &dyn std::fmt::Display| format!("read {config_path:?}: {error}");
        let directory = fs::open(&path, OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
            .map_err(|error| read_error(&error))?;
        let mut file = File::from(
            fs::openat(
                &directory,
                "config.toml",
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
            match &mut entry.kind {
                Kind::Template {
                    source,
                    destination,
                } => {
                    if entry.privileged {
                        return Err(invalid("privileged template installs are not supported"));
                    }
                    if source.is_absolute()
                        || source.file_name().is_none()
                        || source.as_os_str().as_encoded_bytes().contains(&0)
                        || source.components().any(|part| part == Component::ParentDir)
                    {
                        return Err(invalid("source must be a file path inside templates/"));
                    }
                    *destination = expand_destination(destination, &config_home)
                        .map_err(|reason| invalid(&reason))?;
                }
                Kind::Login { command } => {
                    if entry.privileged {
                        return Err(invalid("login entries cannot be privileged"));
                    }
                    if command.first().is_none_or(String::is_empty) {
                        return Err(invalid("command must start with an executable"));
                    }
                    if command.iter().any(|argument| argument.contains('\0')) {
                        return Err(invalid("command must not contain NUL characters"));
                    }
                }
            }
        }
        Ok(Self {
            directory,
            path,
            entries,
        })
    }
}

fn home() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| "HOME must be an absolute path".into())
}

fn expand_destination(path: &Path, config_home: &Path) -> Result<PathBuf, String> {
    let value = path.to_str().expect("TOML paths are UTF-8");
    let expanded = if let Some(rest) = value.strip_prefix("~/") {
        home()?.join(rest.trim_start_matches('/'))
    } else if let Some(rest) = value.strip_prefix("$XDG_CONFIG_HOME/") {
        config_home.join(rest.trim_start_matches('/'))
    } else {
        path.to_owned()
    };
    if !expanded.is_absolute() || expanded.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(
            "destination must be an absolute path or start with ~/ or $XDG_CONFIG_HOME/".into(),
        );
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
