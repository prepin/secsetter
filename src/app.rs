use std::fmt;

use crate::config::{Config, Entry, Kind};
use crate::paths::{self, Location, PathError};

pub(crate) enum Action {
    Install,
}

pub(crate) enum Status {
    Installed,
    NotInstalled,
    Blocked(PathError),
    Unknown(PathError),
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Installed => f.write_str("Installed"),
            Self::NotInstalled => f.write_str("Not installed"),
            Self::Blocked(error) => write!(f, "Blocked: {error}"),
            Self::Unknown(error) => write!(f, "Unknown: {error}"),
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
        let Kind::Template { destination, .. } = &entry.kind;
        match Location::destination(destination, false).and_then(|file| file.regular_file()) {
            Ok(_) => Status::Installed,
            Err(PathError::Missing(_)) => Status::NotInstalled,
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

    pub fn perform(&self, id: &str, action: Action) -> Result<&'static str, String> {
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
        }
    }
}
