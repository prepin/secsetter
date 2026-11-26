mod json;
mod toml;
mod yaml;

use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde::Deserialize;

use crate::paths::{Location, PathError};

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Format {
    Json,
    Toml,
    Yaml,
}

enum EditError {
    Blocked(&'static str),
    MissingParent(usize),
}

impl Format {
    pub fn is_set(self, destination: &Path, key: &[String]) -> Result<bool, PathError> {
        let mut file = Location::destination(destination, false)?.open_regular()?;
        let document = read_document(&mut file, destination)?;
        let result = match self {
            Self::Json => json::is_set(document, key),
            Self::Toml => toml::is_set(document, key),
            Self::Yaml => yaml::is_set(document, key),
        };
        result.map_err(|reason| PathError::Blocked(destination.to_owned(), reason))
    }

    pub fn write(self, destination: &Path, key: &[String], bytes: &[u8]) -> Result<(), String> {
        let value = str::from_utf8(bytes)
            .map_err(|_| format!("{destination:?}: secret input must be valid UTF-8"))?;
        let file_error = |error: PathError| match error {
            PathError::Missing(_) => {
                format!("{destination:?} requires an existing file: {error}")
            }
            PathError::Blocked(..) => format!("Blocked: {error}"),
            _ => error.to_string(),
        };
        let location = Location::destination(destination, false).map_err(file_error)?;
        let locked = location.lock().map_err(file_error)?;
        let mut file = location.open_regular().map_err(file_error)?;
        let document = read_document(&mut file, destination).map_err(file_error)?;
        let edited = match self {
            Self::Json => json::edit(document, key, value),
            Self::Toml => toml::edit(document, key, value),
            Self::Yaml => yaml::edit(document, key, value),
        }
        .map_err(|error| match error {
            EditError::Blocked(reason) => format!("Blocked: {destination:?}: {reason}"),
            EditError::MissingParent(0) => {
                format!("{destination:?}: the root object does not exist")
            }
            EditError::MissingParent(count) => {
                format!(
                    "{destination:?}: parent object {:?} does not exist",
                    &key[..count]
                )
            }
        })?;
        locked
            .replace_file(&file, edited.as_bytes())
            .map_err(file_error)
    }
}

fn read_document(file: &mut File, path: &Path) -> Result<String, PathError> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| PathError::Io(path.to_owned(), "read", error))?;
    String::from_utf8(bytes)
        .map_err(|_| PathError::Blocked(path.to_owned(), "document is not valid UTF-8"))
}
