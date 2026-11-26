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
        if let Self::Field { key, .. } = self {
            validate_key(key).map_err(|error| format!("{path:?}: {error}"))?;
        }
        let _interrupts =
            WriteInterrupts::start().map_err(|error| format!("set up interruption: {error}"))?;
        let location = match ownership {
            Some(ownership) => Location::privileged(path, ownership),
            None => Location::destination(path, !field),
        }
        .map_err(|error| {
            if field {
                error.for_field_write(path)
            } else {
                error.for_write()
            }
        })?;
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
        }
        Ok(Outcome::Written)
    }
}
