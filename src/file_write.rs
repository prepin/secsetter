use std::path::Path;

use rustix::process::{Gid, Uid};
use serde::{Deserialize, Serialize};

use crate::paths::{Location, PathError};
use crate::secret_field::Format;

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum FileWrite {
    Create,
    Replace,
    Field { format: Format, key: Vec<String> },
}

#[derive(PartialEq, Eq)]
pub(crate) enum Outcome {
    Written,
    AlreadyExists,
}

impl FileWrite {
    pub fn apply(
        &self,
        path: &Path,
        ownership: Option<(Uid, Gid)>,
        bytes: &[u8],
    ) -> Result<Outcome, String> {
        let location = match ownership {
            Some(ownership) => Location::privileged(path, ownership),
            None => Location::destination(path, !matches!(self, Self::Field { .. })),
        }
        .map_err(|error| match error {
            PathError::Missing(_) if matches!(self, Self::Field { .. }) => {
                format!("{path:?} requires an existing file: {error}")
            }
            _ => write_error(error),
        })?;
        match self {
            Self::Create => {
                if !location.create_file(bytes).map_err(write_error)? {
                    return Ok(Outcome::AlreadyExists);
                }
            }
            Self::Replace => location
                .lock()
                .and_then(|locked| locked.write_file(bytes))
                .map_err(write_error)?,
            Self::Field { format, key } => format.write(&location, key, bytes)?,
        }
        Ok(Outcome::Written)
    }
}

pub(crate) fn write_error(error: PathError) -> String {
    match error {
        PathError::Blocked(..) => format!("Blocked: {error}"),
        _ => error.to_string(),
    }
}
