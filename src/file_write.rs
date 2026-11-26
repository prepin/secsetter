use std::path::Path;

use rustix::process::{Gid, Uid};
use serde::{Deserialize, Serialize};

use crate::paths::{Location, PathError, WriteInterrupts};
use crate::secret_field::{Format, validate_key};

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum FileWrite {
    Create,
    Replace,
    Field { format: Format, key: Vec<String> },
    Remove,
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
        let field = matches!(self, Self::Field { .. });
        let clearing = bytes.is_empty() && (field || matches!(self, Self::Remove));
        match self {
            Self::Replace if bytes.is_empty() => return Err("secret input is empty".into()),
            Self::Remove if !bytes.is_empty() => return Err("unexpected input for removal".into()),
            _ => {}
        }
        if let Self::Field { key, .. } = self {
            validate_key(key).map_err(|error| format!("{path:?}: {error}"))?;
        }
        let _interrupts =
            WriteInterrupts::start().map_err(|error| format!("set up interruption: {error}"))?;
        let location = match ownership {
            Some(ownership) => Location::privileged(path, ownership),
            None => Location::destination(path, matches!(self, Self::Create | Self::Replace)),
        };
        let location = match location {
            Ok(location) => location,
            Err(PathError::Missing(_)) if clearing => return Ok(Outcome::Written),
            Err(error) if field => return Err(error.for_field_write(path)),
            Err(error) => return Err(error.for_write()),
        };
        match self {
            Self::Create => {
                if !location.create_file(bytes).map_err(PathError::for_write)? {
                    return Ok(Outcome::AlreadyExists);
                }
            }
            Self::Replace => location
                .lock()
                .and_then(|locked| locked.write_file(bytes))
                .map_err(PathError::for_write)?,
            Self::Field { format, key } => format.write(&location, key, bytes)?,
            Self::Remove => location
                .lock()
                .and_then(|locked| locked.remove_file())
                .map_err(PathError::for_write)?,
        }
        Ok(Outcome::Written)
    }
}
