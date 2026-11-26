use std::fmt;
use std::io::Read;
use std::path::Path;

use crate::config::{Config, Entry, Kind, LoginCheck};
use crate::file_write::{FileWrite, Outcome};
use crate::fnox;
use crate::paths::{self, Location, PathError};
use crate::privileged;

pub(crate) enum Action<'a> {
    Install,
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
            Kind::Login { ready_when, .. } => return login_status(ready_when),
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
                Action::Install,
                Kind::Template {
                    source,
                    destination,
                },
            ) => {
                match self.status(entry) {
                    Status::Installed => return Ok("Already installed"),
                    Status::NotInstalled => {}
                    Status::Unknown(_) if entry.ownership().is_some() => {}
                    Status::Blocked(error) => {
                        return Err(format!("install {id:?}: Blocked: {error}"));
                    }
                    Status::Unknown(error) => return Err(format!("install {id:?}: {error}")),
                    _ => unreachable!("template has a template status"),
                }
                let error = |error: String| format!("install {id:?}: {error}");
                let mut input =
                    paths::open_template(&self.config.directory, &self.config.path, source)
                        .map_err(|cause| error(cause.to_string()))?;
                let mut bytes = Vec::new();
                input
                    .read_to_end(&mut bytes)
                    .map_err(|cause| error(format!("read template {source:?}: {cause}")))?;
                match write(entry, destination, FileWrite::Create, &bytes).map_err(error)? {
                    Outcome::Written => Ok("Installed"),
                    Outcome::AlreadyExists => Ok("Already installed"),
                }
            }
            (
                Action::Restore { replace },
                Kind::FnoxFile {
                    source,
                    destination,
                },
            ) => restore_file(entry, source, destination, replace),
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
            (Action::Install, _) => Err(format!("install does not match entry {id:?}")),
            (Action::Set(_), _) => Err(format!("set does not match entry {id:?}")),
            (Action::Restore { .. }, _) => Err(format!("restore does not match entry {id:?}")),
        }
    }
}

fn login_status(checks: &[LoginCheck]) -> Status {
    if checks.is_empty() {
        return Status::Untracked;
    }

    let mut unknown = None;
    for check in checks {
        let result = match check {
            LoginCheck::FileExists { path } => Location::destination(path, false)
                .and_then(|location| location.regular_file())
                .map(|_| true),
            LoginCheck::FieldSet { path, format, key } => format.is_set(path, key),
        };
        match result {
            Ok(true) => {}
            Ok(false) | Err(PathError::Missing(_)) => return Status::LoginNeeded,
            Err(error) => {
                unknown.get_or_insert(error);
            }
        }
    }
    unknown.map_or(Status::LoginConfigured, Status::Unknown)
}

pub(crate) enum RestoreDestination {
    Present,
    Missing,
    RequiresElevation,
}

pub(crate) fn restore_destination(
    entry: &Entry,
    destination: &Path,
) -> Result<RestoreDestination, PathError> {
    match Location::destination(destination, false).and_then(|location| location.regular_file()) {
        Ok(_) => Ok(RestoreDestination::Present),
        Err(PathError::Missing(_)) => Ok(RestoreDestination::Missing),
        Err(PathError::Io(..)) if entry.ownership().is_some() => {
            Ok(RestoreDestination::RequiresElevation)
        }
        Err(error) => Err(error),
    }
}

fn restore_file(
    entry: &Entry,
    source: &str,
    destination: &Path,
    replace: bool,
) -> Result<&'static str, String> {
    let id = &entry.id;
    let restore_error = |error: String| format!("restore {id:?}: {error}");
    let needs_approval = || {
        format!(
            "restore {id:?}: destination already exists; confirm replacement or use --replace: {destination:?}"
        )
    };
    let state = restore_destination(entry, destination)
        .map_err(|error| restore_error(error.for_write()))?;
    if matches!(state, RestoreDestination::Present) && !replace {
        return Err(needs_approval());
    }
    let bytes = fnox::fetch(source).map_err(restore_error)?;
    let operation = if replace {
        FileWrite::Replace
    } else {
        FileWrite::Create
    };
    if write(entry, destination, operation, &bytes).map_err(restore_error)?
        == Outcome::AlreadyExists
    {
        return Err(needs_approval());
    }
    Ok("Restored")
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
