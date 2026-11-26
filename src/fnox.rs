use std::io;
use std::process::{Command, Stdio};

pub(crate) fn fetch(source: &str) -> Result<Vec<u8>, String> {
    let mut command = Command::new("fnox");
    command.arg("get");
    if source.starts_with('-') {
        command.arg(format!("--file={source}"));
    } else {
        command.args(["--file", source]);
    }
    let output = command.stderr(Stdio::null()).output().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            format!("fnox get --file {source:?}: fnox executable not found")
        } else {
            format!("fnox get --file {source:?}: {error}")
        }
    })?;
    if !output.status.success() {
        return Err(format!(
            "fnox get --file {source:?} failed ({})",
            output.status
        ));
    }
    if output.stdout.is_empty() {
        return Err(format!("fnox get --file {source:?}: empty output"));
    }
    Ok(output.stdout)
}
