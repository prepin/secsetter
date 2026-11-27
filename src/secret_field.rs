mod json;
mod netrc;
mod toml;
mod yaml;

use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::paths::{Location, PathError};

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Format {
    Json,
    Netrc,
    Toml,
    Yaml,
}

enum EditError {
    Blocked(&'static str),
    MissingParent(usize),
}

impl Format {
    pub fn validate_key(self, key: &[String]) -> Result<(), &'static str> {
        if matches!(self, Self::Netrc) {
            return netrc::validate_key(key);
        }
        if key.is_empty() {
            return Err("key must contain at least one name");
        }
        Ok(())
    }

    pub fn is_set(self, destination: &Path, key: &[String]) -> Result<bool, PathError> {
        let mut file = Location::destination(destination, false)?.open_regular()?;
        let document = read_document(&mut file, destination)?;
        self.document_is_set(document, key)
            .map_err(|reason| PathError::Blocked(destination.to_owned(), reason))
    }

    fn document_is_set(self, document: String, key: &[String]) -> Result<bool, &'static str> {
        match self {
            Self::Json => json::is_set(document, key),
            Self::Netrc => netrc::is_set(&document, &key[0]),
            Self::Toml => toml::is_set(document, key),
            Self::Yaml => yaml::is_set(document, key),
        }
    }

    pub fn write(
        self,
        location: &Location<'_>,
        key: &[String],
        bytes: &[u8],
    ) -> Result<(), String> {
        let destination = location.path();
        let value = str::from_utf8(bytes)
            .map_err(|_| format!("{destination:?}: secret input must be valid UTF-8"))?;
        let file_error = |error: PathError| error.for_field_write(destination);
        let locked = location.lock().map_err(file_error)?;
        let mut file = match location.open_regular() {
            Ok(file) => file,
            Err(PathError::Missing(_)) if value.is_empty() => return Ok(()),
            Err(error) => return Err(file_error(error)),
        };
        let document = read_document(&mut file, destination).map_err(file_error)?;
        if !matches!(self, Self::Netrc)
            && value.is_empty()
            && !self
                .document_is_set(document.clone(), key)
                .map_err(|reason| file_error(PathError::Blocked(destination.to_owned(), reason)))?
        {
            return Ok(());
        }
        let edited = match self {
            Self::Json => json::edit(document, key, value).map(Some),
            Self::Netrc => netrc::edit(document, &key[0], value),
            Self::Toml => toml::edit(document, key, value).map(Some),
            Self::Yaml => yaml::edit(document, key, value).map(Some),
        }
        .map_err(|error| match error {
            EditError::Blocked(reason) => {
                PathError::Blocked(destination.to_owned(), reason).for_write()
            }
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
        if let Some(edited) = edited {
            locked
                .replace_file(&file, edited.as_bytes())
                .map_err(file_error)?;
        }
        Ok(())
    }
}

fn read_document(file: &mut File, path: &Path) -> Result<String, PathError> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| PathError::Io(path.to_owned(), "read", error))?;
    String::from_utf8(bytes)
        .map_err(|_| PathError::Blocked(path.to_owned(), "document is not valid UTF-8"))
}
