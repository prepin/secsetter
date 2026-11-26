use std::fmt;
use std::io::Read;

use crate::config::{Config, Entry, Kind};
use crate::paths::{self, Location, PathError};

pub(crate) enum Action<'a> {
    Install,
    Set(&'a mut dyn Read),
}

pub(crate) enum Status {
    Installed,
    NotInstalled,
    Set,
    NotSet,
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
    pub fn load() -> Result<Self, String> {
        Ok(Self {
            config: Config::load()?,
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
            Kind::SecretFile { destination } => (destination, false),
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
            Kind::Login { .. } => return Status::Untracked,
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

    pub fn login_command(&self, id: &str) -> Result<&[String], String> {
        match &self.entry(id)?.kind {
            Kind::Login { command } => Ok(command),
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
                    Status::Blocked(error) => {
                        return Err(format!("install {id:?}: Blocked: {error}"));
                    }
                    Status::Unknown(error) => return Err(format!("install {id:?}: {error}")),
                    _ => unreachable!("template has a template status"),
                }
                let install = || {
                    let input =
                        paths::open_template(&self.config.directory, &self.config.path, source)?;
                    Location::destination(destination, true)?.create_file(input)
                };
                match install() {
                    Ok(true) => Ok("Installed"),
                    Ok(false) => Ok("Already installed"),
                    Err(error) => Err(format!("install {id:?}: {error}")),
                }
            }
            (Action::Set(input), kind @ (Kind::SecretFile { .. } | Kind::SecretField { .. })) => {
                let mut bytes = Vec::new();
                input
                    .read_to_end(&mut bytes)
                    .map_err(|error| format!("set {id:?}: read secret input: {error}"))?;
                if bytes.is_empty() {
                    return Err(format!("set {id:?}: secret input is empty"));
                }
                match kind {
                    Kind::SecretFile { destination } => {
                        let write = || {
                            Location::destination(destination, true)?
                                .lock()?
                                .write_file(&bytes[..])
                        };
                        write().map_err(|error| match error {
                            PathError::Blocked(..) => format!("set {id:?}: Blocked: {error}"),
                            _ => format!("set {id:?}: {error}"),
                        })?;
                    }
                    Kind::SecretField {
                        destination,
                        format,
                        key,
                    } => format
                        .write(destination, key, &bytes)
                        .map_err(|error| format!("set {id:?}: {error}"))?,
                    _ => unreachable!("set requires a manual secret entry"),
                }
                Ok("Set")
            }
            (Action::Install, _) => Err(format!("install does not match entry {id:?}")),
            (Action::Set(_), _) => Err(format!("set does not match entry {id:?}")),
        }
    }
}
