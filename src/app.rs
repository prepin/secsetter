use std::fmt;
use std::io::Read;
use std::path::Path;

use crate::config::{Config, Entry, Kind, LoginCheck};
use crate::file_write::{FileWrite, Outcome};
use crate::fnox;
use crate::login;
use crate::paths::{self, Location, PathError};
use crate::privileged;

pub(crate) enum Action<'a> {
    Install { replace: bool },
    Set(&'a mut dyn Read),
    Restore { replace: bool },
    Clear,
}

pub(crate) enum Status {
    Installed,
    NotInstalled,
    Set,
    NotSet,
    LoginConfigured,
    LoginNeeded,
    Blocked(PathError),
    Unknown(PathError),
    CheckError(String),
    Checking,
    Untracked,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Installed => f.write_str("Installed"),
            Self::NotInstalled => f.write_str("Not installed"),
            Self::Set => f.write_str("Set"),
            Self::NotSet => f.write_str("Not set"),
            Self::LoginConfigured => f.write_str("Login configured"),
            Self::LoginNeeded => f.write_str("Login needed"),
            Self::Blocked(error) => write!(f, "Blocked: {error}"),
            Self::Unknown(error) => write!(f, "Unknown: {error}"),
            Self::CheckError(error) => write!(f, "Unknown: {error}"),
            Self::Checking => f.write_str("Checking"),
            Self::Untracked => Ok(()),
        }
    }
}

pub(crate) struct App {
    config: Config,
}

impl App {
    pub fn load(path: Option<&Path>) -> Result<Self, String> {
        Ok(Self {
            config: Config::load(path)?,
        })
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.config
            .entries
            .iter()
            .filter(|entry| entry.applies_here())
    }

    pub fn status(&self, entry: &Entry) -> Status {
        let (destination, template) = match &entry.kind {
            Kind::Template { destination, .. } => (destination, true),
            Kind::SecretFile { destination } | Kind::FnoxFile { destination, .. } => {
                (destination, false)
            }
            Kind::SecretField {
                destination,
                format,
                key,
            } => {
                return match format.is_set(destination, key) {
                    Ok(true) => Status::Set,
                    Ok(false) | Err(PathError::Missing(_)) => Status::NotSet,
                    Err(error @ PathError::Blocked(..)) => Status::Blocked(error),
                    Err(error) => Status::Unknown(error),
                };
            }
            Kind::Login { ready_when, .. } if entry.command_readiness().is_none() => {
                return login_status(ready_when, || false);
            }
            Kind::Login { .. } => return Status::Checking,
        };
        match Location::destination(destination, false).and_then(|file| file.regular_file()) {
            Ok(_) if template => Status::Installed,
            Ok(stat) if stat.st_size != 0 => Status::Set,
            Err(PathError::Missing(_)) if template => Status::NotInstalled,
            Ok(_) | Err(PathError::Missing(_)) => Status::NotSet,
            Err(error @ PathError::Blocked(..)) => Status::Blocked(error),
            Err(error) => Status::Unknown(error),
        }
    }

    pub fn entry(&self, id: &str) -> Result<&Entry, String> {
        let entry = self
            .config
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| format!("unknown entry ID {id:?}"))?;
        if !entry.applies_here() {
            return Err(format!("entry {id:?} does not apply to this platform"));
        }
        Ok(entry)
    }

    pub fn login_commands(&self, id: &str) -> Result<&[Vec<String>], String> {
        match &self.entry(id)?.kind {
            Kind::Login { commands, .. } => Ok(commands),
            _ => Err(format!("login does not match entry {id:?}")),
        }
    }

    pub fn perform(&self, id: &str, action: Action<'_>) -> Result<&'static str, String> {
        let entry = self.entry(id)?;
        match (action, &entry.kind) {
            (
                Action::Install { replace },
                Kind::Template {
                    source,
                    destination,
                },
            ) => {
                let operation = if replace {
                    FileWrite::ReplaceTemplate
                } else {
                    FileWrite::Create
                };
                write_from_source(entry, destination, operation, "install", || {
                    let mut input =
                        paths::open_template(&self.config.directory, &self.config.path, source)
                            .map_err(|error| error.to_string())?;
                    let mut bytes = Vec::new();
                    input
                        .read_to_end(&mut bytes)
                        .map_err(|error| format!("read template {source:?}: {error}"))?;
                    Ok(bytes)
                })?;
                Ok("Installed")
            }
            (
                Action::Restore { replace },
                Kind::FnoxFile {
                    source,
                    destination,
                },
            ) => {
                let operation = if replace {
                    FileWrite::Replace
                } else {
                    FileWrite::Create
                };
                write_from_source(entry, destination, operation, "restore", || {
                    fnox::fetch(source)
                })?;
                Ok("Restored")
            }
            (Action::Set(input), kind @ (Kind::SecretFile { .. } | Kind::SecretField { .. })) => {
                let mut bytes = Vec::new();
                input
                    .read_to_end(&mut bytes)
                    .map_err(|error| format!("set {id:?}: read secret input: {error}"))?;
                if bytes.is_empty() {
                    return Err(format!("set {id:?}: secret input is empty"));
                }
                let (destination, operation) = match kind {
                    Kind::SecretFile { destination } => (destination, FileWrite::Replace),
                    Kind::SecretField {
                        destination,
                        format,
                        key,
                    } => (
                        destination,
                        FileWrite::Field {
                            format: *format,
                            key: key.clone(),
                        },
                    ),
                    _ => unreachable!("set requires a manual secret entry"),
                };
                write(entry, destination, operation, &bytes)
                    .map_err(|error| format!("set {id:?}: {error}"))?;
                Ok("Set")
            }
            (Action::Clear, kind) => {
                let (destination, operation) = match kind {
                    Kind::SecretFile { destination } | Kind::FnoxFile { destination, .. } => {
                        (destination, FileWrite::Remove)
                    }
                    Kind::SecretField {
                        destination,
                        format,
                        key,
                    } => (
                        destination,
                        FileWrite::Field {
                            format: *format,
                            key: key.clone(),
                        },
                    ),
                    _ => return Err(format!("clear does not match entry {id:?}")),
                };
                write(entry, destination, operation, &[])
                    .map_err(|error| format!("clear {id:?}: {error}"))?;
                Ok("Cleared")
            }
            (Action::Install { .. }, _) => Err(format!("install does not match entry {id:?}")),
            (Action::Set(_), _) => Err(format!("set does not match entry {id:?}")),
            (Action::Restore { .. }, _) => Err(format!("restore does not match entry {id:?}")),
        }
    }
}

pub(crate) fn login_status(checks: &[LoginCheck], cancelled: impl Fn() -> bool) -> Status {
    if checks.is_empty() {
        return Status::Untracked;
    }

    let file_status = |result| match result {
        Err(PathError::Missing(_)) => Ok(false),
        result => result.map_err(Status::Unknown),
    };
    let mut unknown = None;
    for (index, check) in checks.iter().enumerate() {
        let result = match check {
            LoginCheck::FileExists { path } => file_status(
                Location::destination(path, false)
                    .and_then(|location| location.regular_file())
                    .map(|_| true),
            ),
            LoginCheck::FieldSet { path, format, key } => file_status(format.is_set(path, key)),
            LoginCheck::Command { command, json } => {
                login::check(command, json.as_ref(), &cancelled).map_err(|error| {
                    Status::CheckError(format!("ready_when command {}: {error}", index + 1))
                })
            }
        };
        match result {
            Ok(true) => {}
            Ok(false) => return Status::LoginNeeded,
            Err(status) => {
                unknown.get_or_insert(status);
            }
        }
    }
    unknown.unwrap_or(Status::LoginConfigured)
}

pub(crate) enum CopyDestination {
    Present,
    Missing,
    RequiresElevation,
}

pub(crate) fn copy_destination(
    entry: &Entry,
    destination: &Path,
) -> Result<CopyDestination, PathError> {
    match Location::destination(destination, false).and_then(|location| location.regular_file()) {
        Ok(_) => Ok(CopyDestination::Present),
        Err(PathError::Missing(_)) => Ok(CopyDestination::Missing),
        Err(PathError::Io(..)) if entry.ownership().is_some() => {
            Ok(CopyDestination::RequiresElevation)
        }
        Err(error) => Err(error),
    }
}

fn write_from_source(
    entry: &Entry,
    destination: &Path,
    operation: FileWrite,
    command: &str,
    read: impl FnOnce() -> Result<Vec<u8>, String>,
) -> Result<(), String> {
    let id = &entry.id;
    let action_error = |error: String| format!("{command} {id:?}: {error}");
    let needs_approval = || {
        format!(
            "{command} {id:?}: destination already exists; confirm replacement or use --replace: {destination:?}"
        )
    };
    let state =
        copy_destination(entry, destination).map_err(|error| action_error(error.for_write()))?;
    if matches!(state, CopyDestination::Present) && matches!(operation, FileWrite::Create) {
        return Err(needs_approval());
    }
    let bytes = read().map_err(action_error)?;
    if write(entry, destination, operation, &bytes).map_err(action_error)? == Outcome::AlreadyExists
    {
        return Err(needs_approval());
    }
    Ok(())
}

fn write(
    entry: &Entry,
    destination: &Path,
    operation: FileWrite,
    bytes: &[u8],
) -> Result<Outcome, String> {
    match entry.ownership() {
        Some(ownership) => privileged::write(&operation, destination, ownership, bytes),
        None => operation.apply(destination, None, bytes),
    }
}
