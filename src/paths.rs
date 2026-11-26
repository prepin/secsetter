use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{self, AtFlags, CWD, FileType, FlockOperation, Mode, OFlags, RenameFlags};
use rustix::io::Errno;
use rustix::process::Uid;

const PRIVATE_DIR: Mode = Mode::RWXU;
const PRIVATE_FILE: Mode = Mode::RUSR.union(Mode::WUSR);

#[derive(Debug)]
pub(crate) enum PathError {
    Missing(PathBuf),
    Blocked(PathBuf, &'static str),
    Io(PathBuf, &'static str, io::Error),
}

impl PathError {
    fn lookup(path: &Path, cause: Errno) -> Self {
        match cause {
            Errno::NOENT => Self::Missing(path.to_owned()),
            Errno::NOTDIR | Errno::LOOP => {
                Self::Blocked(path.to_owned(), "symbolic link or non-directory component")
            }
            _ => Self::io(path, "inspect", cause),
        }
    }

    fn io(path: &Path, operation: &'static str, error: impl Into<io::Error>) -> Self {
        Self::Io(path.to_owned(), operation, error.into())
    }
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(path) => write!(f, "{path:?} does not exist"),
            Self::Blocked(path, reason) => write!(f, "{path:?}: {reason}"),
            Self::Io(path, operation, cause) => write!(f, "{operation} {path:?}: {cause}"),
        }
    }
}

pub(crate) struct Location<'a> {
    parent: OwnedFd,
    name: &'a OsStr,
    path: PathBuf,
}

impl<'a> Location<'a> {
    pub fn destination(path: &'a Path, create_parents: bool) -> Result<Self, PathError> {
        if ends_as_directory(path) {
            return Err(PathError::Blocked(
                path.to_owned(),
                "expected a regular file",
            ));
        }
        let root = directory(CWD, "/").map_err(|error| PathError::lookup(Path::new("/"), error))?;
        let relative = path.strip_prefix("/").expect("absolute destination");
        Self::within(root, Path::new("/"), relative, create_parents)
    }

    fn within(
        mut parent: OwnedFd,
        base: &Path,
        relative: &'a Path,
        create_parents: bool,
    ) -> Result<Self, PathError> {
        let path = base.join(relative);
        let not_file = || PathError::Blocked(path.clone(), "expected a regular file");
        let name = relative.file_name().ok_or_else(not_file)?;
        if ends_as_directory(relative) {
            return Err(not_file());
        }
        let mut component_path = base.to_owned();
        for component in relative.parent().expect("file has a parent").components() {
            let component = match component {
                Component::Normal(name) => name,
                Component::ParentDir => OsStr::new(".."),
                _ => continue,
            };
            component_path.push(component);
            // Each lookup is one name under a pinned directory, including after mkdirat.
            let mut next = directory(&parent, component);
            if matches!(next, Err(Errno::NOENT)) && create_parents {
                match fs::mkdirat(&parent, component, PRIVATE_DIR) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => {
                        return Err(PathError::io(&component_path, "create directory", error));
                    }
                }
                next = directory(&parent, component);
            }
            parent = next.map_err(|error| PathError::lookup(&component_path, error))?;
        }
        Ok(Self { parent, name, path })
    }

    pub fn regular_file(&self) -> Result<fs::Stat, PathError> {
        let stat = fs::statat(&self.parent, self.name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| PathError::lookup(&self.path, error))?;
        self.require_regular(FileType::from_raw_mode(stat.st_mode))?;
        Ok(stat)
    }

    fn require_regular(&self, kind: FileType) -> Result<(), PathError> {
        match kind {
            FileType::RegularFile => Ok(()),
            FileType::Symlink => Err(PathError::Blocked(self.path.clone(), "symbolic link")),
            _ => Err(PathError::Blocked(
                self.path.clone(),
                "expected a regular file",
            )),
        }
    }

    pub fn open_regular(&self) -> Result<File, PathError> {
        self.regular_file()?;
        let file = fs::openat(
            &self.parent,
            self.name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| PathError::lookup(&self.path, error))?;
        let stat = fs::fstat(&file).map_err(|error| PathError::io(&self.path, "inspect", error))?;
        self.require_regular(FileType::from_raw_mode(stat.st_mode))?;
        Ok(File::from(file))
    }

    pub fn lock(&self) -> Result<LockedLocation<'_, 'a>, PathError> {
        let lock_error = |error| PathError::io(&self.path, "lock directory for", error);
        // Lock the pinned directory: its inode stays stable when files are replaced.
        let guard = fs::openat(
            &self.parent,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(lock_error)?;
        loop {
            match fs::flock(&guard, FlockOperation::LockExclusive) {
                Ok(()) => break,
                Err(Errno::INTR) => continue,
                Err(error) => return Err(lock_error(error)),
            }
        }
        Ok(LockedLocation {
            location: self,
            _guard: guard,
        })
    }

    pub fn create_file(&self, input: impl Read) -> Result<bool, PathError> {
        let stage = self.stage_file(input, None)?;
        match stage.commit_new(self) {
            Ok(()) => Ok(true),
            Err(Errno::EXIST) => {
                self.regular_file()?;
                Ok(false)
            }
            Err(error) => Err(PathError::io(&self.path, "install", error)),
        }
    }

    fn stage_file(
        &self,
        mut input: impl Read,
        owner: Option<Uid>,
    ) -> Result<StagingDir<'_>, PathError> {
        let stage = StagingDir::new(self)?;
        let write_error = |error| PathError::io(&self.path, "write temporary file for", error);
        let mut file = File::from(
            fs::openat(
                &stage.fd,
                "file",
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                PRIVATE_FILE,
            )
            .map_err(|error| write_error(error.into()))?,
        );
        io::copy(&mut input, &mut file).map_err(write_error)?;
        if let Some(owner) = owner.filter(|&owner| owner != rustix::process::geteuid()) {
            fs::fchown(&file, Some(owner), None).map_err(|error| write_error(error.into()))?;
        }
        fs::fchmod(&file, PRIVATE_FILE).map_err(|error| write_error(error.into()))?;
        file.sync_all().map_err(write_error)?;
        Ok(stage)
    }
}

pub(crate) struct LockedLocation<'location, 'path> {
    location: &'location Location<'path>,
    _guard: OwnedFd,
}

impl LockedLocation<'_, '_> {
    pub fn write_file(&self, input: impl Read) -> Result<(), PathError> {
        let location = self.location;
        let existing = match location.regular_file() {
            Ok(stat) => Some(stat),
            Err(PathError::Missing(_)) => None,
            Err(error) => return Err(error),
        };
        self.commit_file(input, existing)
    }

    pub fn replace_file(&self, original: &File, input: impl Read) -> Result<(), PathError> {
        // A field edit may only replace the file it read.
        let existing = fs::fstat(original)
            .map_err(|error| PathError::io(&self.location.path, "inspect", error))?;
        self.commit_file(input, Some(existing))
    }

    fn commit_file(&self, input: impl Read, existing: Option<fs::Stat>) -> Result<(), PathError> {
        let location = self.location;
        let owner = existing.map(|stat| Uid::from_raw(stat.st_uid));
        let stage = location.stage_file(input, owner)?;

        let commit = if let Some(existing) = existing {
            let current = location.regular_file()?;
            if (current.st_dev, current.st_ino, current.st_uid)
                != (existing.st_dev, existing.st_ino, existing.st_uid)
            {
                return Err(PathError::Blocked(
                    location.path.clone(),
                    "destination changed during write",
                ));
            }
            fs::renameat(&stage.fd, "file", &location.parent, location.name)
        } else {
            stage.commit_new(location)
        };
        if commit == Err(Errno::EXIST) {
            location.regular_file()?;
        }
        commit.map_err(|error| PathError::io(&location.path, "commit write for", error))
    }
}

pub(crate) fn open_template(
    config_dir: &OwnedFd,
    config_path: &Path,
    path: &Path,
) -> Result<File, PathError> {
    let base = config_path.join("templates");
    let root =
        directory(config_dir, "templates").map_err(|error| PathError::lookup(&base, error))?;
    Location::within(root, &base, path, false)?.open_regular()
}

fn ends_as_directory(path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    bytes.ends_with(b"/") || bytes.ends_with(b"/.")
}

fn directory(parent: impl AsFd, name: impl rustix::path::Arg) -> Result<OwnedFd, Errno> {
    #[cfg(target_os = "linux")]
    let access = OFlags::PATH;
    #[cfg(target_os = "macos")]
    let access = OFlags::RDONLY;
    fs::openat(
        parent,
        name,
        access | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

struct StagingDir<'a> {
    parent: &'a OwnedFd,
    name: String,
    fd: OwnedFd,
}

impl<'a> StagingDir<'a> {
    fn commit_new(&self, destination: &Location<'_>) -> Result<(), Errno> {
        fs::renameat_with(
            &self.fd,
            "file",
            &destination.parent,
            destination.name,
            RenameFlags::NOREPLACE,
        )
    }

    fn new(destination: &'a Location<'_>) -> Result<Self, PathError> {
        let error = |cause: io::Error| PathError::io(&destination.path, "stage write for", cause);
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|cause| error(io::Error::other(cause.to_string())))?;
        let name = format!(".secsetter-{:032x}", u128::from_ne_bytes(random));
        let parent = &destination.parent;
        // A private directory keeps other users with access to the destination directory
        // from substituting the staged file between creation and commit.
        fs::mkdirat(parent, name.as_str(), PRIVATE_DIR).map_err(|cause| error(cause.into()))?;
        let fd = match directory(parent, name.as_str()) {
            Ok(fd) => fd,
            Err(cause) => {
                let _ = fs::unlinkat(parent, name.as_str(), AtFlags::REMOVEDIR);
                return Err(error(cause.into()));
            }
        };
        let check_private = |stat: fs::Stat| {
            if stat.st_uid == rustix::process::geteuid().as_raw() && stat.st_mode & 0o077 == 0 {
                Ok(())
            } else {
                Err(io::Error::other("temporary directory is not private"))
            }
        };
        if let Err(cause) = fs::fstat(&fd)
            .map_err(io::Error::from)
            .and_then(check_private)
        {
            let _ = fs::unlinkat(parent, name.as_str(), AtFlags::REMOVEDIR);
            return Err(error(cause));
        }
        let stage = Self { parent, name, fd };
        #[cfg(target_os = "macos")]
        clear_inherited_acl(&stage.fd).map_err(error)?;
        Ok(stage)
    }
}

#[cfg(target_os = "macos")]
fn clear_inherited_acl(directory: &OwnedFd) -> io::Result<()> {
    use std::ffi::{c_int, c_void};
    use std::os::fd::AsRawFd;

    unsafe extern "C" {
        fn acl_init(count: c_int) -> *mut c_void;
        fn acl_set_fd(fd: c_int, acl: *mut c_void) -> c_int;
        fn acl_free(acl: *mut c_void) -> c_int;
    }

    // Darwin ACLs can grant access beyond mode 0700. Clear them before staging data.
    // SAFETY: The ACL is allocated by acl_init, used with a live fd, and freed once.
    unsafe {
        let acl = acl_init(0);
        if acl.is_null() {
            return Err(io::Error::last_os_error());
        }
        let result = if acl_set_fd(directory.as_raw_fd(), acl) == 0 {
            Ok(())
        } else {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Unsupported {
                Ok(())
            } else {
                Err(error)
            }
        };
        acl_free(acl);
        result
    }
}

impl Drop for StagingDir<'_> {
    fn drop(&mut self) {
        let _ = fs::unlinkat(&self.fd, "file", AtFlags::empty());
        let _ = fs::unlinkat(self.parent, self.name.as_str(), AtFlags::REMOVEDIR);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    struct WhileCopying<F>(Option<F>);

    impl<F: FnOnce()> Read for WhileCopying<F> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if let Some(change) = self.0.take() {
                change();
                return b"template".as_slice().read(buffer);
            }
            Ok(0)
        }
    }

    #[test]
    fn keeps_a_destination_created_during_copy() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let path = parent.join("config");
        let location = Location::destination(&path, false).unwrap();

        let created = location.create_file(WhileCopying(Some(|| {
            fs::write(&path, b"concurrent creation").unwrap();
        })));

        assert!(!created.unwrap());
        assert_eq!(fs::read(path).unwrap(), b"concurrent creation");
        assert_eq!(fs::read_dir(parent).unwrap().count(), 1);
    }

    #[test]
    fn blocks_a_destination_link_created_during_copy() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let path = parent.join("config");
        let target = parent.join("target");
        let location = Location::destination(&path, false).unwrap();

        let result = location.create_file(WhileCopying(Some(|| {
            symlink(&target, &path).unwrap();
        })));

        assert!(matches!(result, Err(PathError::Blocked(..))));
        assert!(path.is_symlink());
        assert!(!target.exists());
        assert_eq!(fs::read_dir(parent).unwrap().count(), 1);
    }

    #[test]
    fn does_not_follow_a_parent_link_substituted_during_copy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let parent = root.join("parent");
        let moved = root.join("moved");
        let outside = root.join("outside");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&outside).unwrap();
        let path = parent.join("config");
        let location = Location::destination(&path, false).unwrap();

        let _ = location.create_file(WhileCopying(Some(|| {
            fs::rename(&parent, &moved).unwrap();
            symlink(&outside, &parent).unwrap();
        })));

        assert!(parent.is_symlink());
        assert!(!outside.join("config").exists());
    }

    #[test]
    fn keeps_the_old_secret_and_removes_staging_after_a_partial_read_error() {
        struct ReadError;

        impl Read for ReadError {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("synthetic read failure"))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let path = parent.join("token");
        fs::write(&path, b"old synthetic secret").unwrap();
        let location = Location::destination(&path, false).unwrap();

        let result = location
            .lock()
            .unwrap()
            .write_file(b"partial replacement".as_slice().chain(ReadError));
        assert!(matches!(result, Err(PathError::Io(..))));
        assert_eq!(fs::read(&path).unwrap(), b"old synthetic secret");
        assert_eq!(fs::read_dir(parent).unwrap().count(), 1);
    }

    #[test]
    fn rejects_a_link_substituted_during_a_secret_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let path = parent.join("token");
        let target = parent.join("outside");
        fs::write(&path, b"old synthetic secret").unwrap();
        fs::write(&target, b"unchanged").unwrap();
        let location = Location::destination(&path, false).unwrap();

        let result = location.lock().unwrap().write_file(WhileCopying(Some(|| {
            fs::remove_file(&path).unwrap();
            symlink(&target, &path).unwrap();
        })));
        assert!(matches!(result, Err(PathError::Blocked(..))));
        assert!(path.is_symlink());
        assert_eq!(fs::read(target).unwrap(), b"unchanged");
        assert_eq!(fs::read_dir(parent).unwrap().count(), 2);
    }

    #[test]
    fn keeps_a_new_destination_if_a_field_edit_read_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("config");
        fs::write(&path, b"original config").unwrap();
        let location = Location::destination(&path, false).unwrap();
        let locked = location.lock().unwrap();
        let original = location.open_regular().unwrap();

        fs::remove_file(&path).unwrap();
        fs::write(&path, b"other process's replacement").unwrap();
        let result = locked.replace_file(&original, b"edited config".as_slice());

        assert!(matches!(result, Err(PathError::Blocked(..))));
        assert_eq!(fs::read(path).unwrap(), b"other process's replacement");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
