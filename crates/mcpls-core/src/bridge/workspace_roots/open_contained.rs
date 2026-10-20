//! Opening a validated workspace path and verifying the opened file.
//!
//! [`WorkspaceRoots::validate`] and the open that follows it are separate
//! steps, so a symlink swapped in between would be followed. This module
//! closes that window per platform:
//!
//! - Linux and Android open the file relative to its root directory with
//!   `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`, so an outside target
//!   is never opened; without `openat2` they walk the components with
//!   `O_NOFOLLOW`. The opened handle's own path from `/proc/self/fd` is then
//!   checked against the roots as well.
//! - macOS walks the components with `O_NOFOLLOW` like other Unix systems, so
//!   an outside target is never opened, then asks the handle for its path with
//!   `fcntl(F_GETPATH)` and checks it against the roots.
//! - Windows has no safe handle-to-path API in the dependencies, so every
//!   ancestor directory is pinned (opened without `FILE_SHARE_DELETE`, so it
//!   cannot be renamed or replaced) and the opened file is compared by
//!   identity with a second, non-following open of the same path.
//! - Other Unix systems walk the components with `O_NOFOLLOW` from the root
//!   directory, so no symlink is followed and no outside target is opened.

#[cfg(unix)]
mod beneath;

use std::io;

#[cfg(unix)]
use self::beneath::Beneath;
use super::{WorkspacePath, WorkspaceRoots};
use crate::error::Error;
use crate::util::RegularFile;

impl WorkspaceRoots {
    /// Opens the validated `path` and verifies that the opened file lies under
    /// a canonical root.
    ///
    /// Runs blocking; async callers use the blocking pool. The decision rests
    /// on the open handle, not on the path string, so a component swapped for
    /// a symlink after [`Self::validate`] cannot lead outside the roots.
    ///
    /// # Errors
    ///
    /// [`Error::FileIo`] when the open or the verification cannot run (a
    /// missing file, or `/proc` unavailable on Linux, which fails closed),
    /// [`Error::NotARegularFile`] for anything but a regular file and
    /// [`Error::PathOutsideWorkspace`] when the opened file is not under a
    /// root.
    pub(crate) fn open_contained(&self, path: &WorkspacePath) -> Result<RegularFile, Error> {
        self.open_contained_with(path, || {})
    }
}

#[cfg(any(windows, all(test, any(target_os = "linux", target_os = "android"))))]
fn open_regular(path: &WorkspacePath) -> Result<RegularFile, Error> {
    RegularFile::open(path.as_path()).map_err(|error| error.into_error(path.as_path()))
}

fn file_io(path: &WorkspacePath, source: io::Error) -> Error {
    Error::FileIo {
        path: path.as_path().to_path_buf(),
        source,
    }
}

fn outside(path: &WorkspacePath) -> Error {
    Error::PathOutsideWorkspace(path.as_path().to_path_buf())
}

#[cfg(unix)]
impl WorkspaceRoots {
    /// Runs `open` on the root containing `path` and the path relative to it.
    fn open_under_root(
        &self,
        path: &WorkspacePath,
        open: impl FnOnce(&std::path::Path, &std::path::Path) -> io::Result<Beneath>,
    ) -> Result<Beneath, Error> {
        let (root, relative) = self
            .split_under_root(path.as_path())
            .ok_or_else(|| outside(path))?;
        (!relative.is_empty())
            .ok_or_else(|| Error::NotARegularFile(path.as_path().to_path_buf()))?;
        open(root, relative).map_err(|source| file_io(path, source))
    }
}

/// Turns the outcome of a containment-aware open into a verified regular file.
#[cfg(unix)]
fn regular_from(path: &WorkspacePath, beneath: Beneath) -> Result<RegularFile, Error> {
    match beneath {
        Beneath::Opened(file) => {
            RegularFile::from_file(file).map_err(|error| error.into_error(path.as_path()))
        }
        Beneath::Escaped => Err(outside(path)),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Beneath::Unsupported => Err(file_io(path, io::ErrorKind::Unsupported.into())),
    }
}

/// A file unlinked since the open still reports its old path; it is gone, not
/// outside the roots.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn require_linked(path: &WorkspacePath, file: &RegularFile) -> Result<(), Error> {
    use std::os::unix::fs::MetadataExt as _;

    let links = file
        .file()
        .metadata()
        .map_err(|source| file_io(path, source))?
        .nlink();
    (links != 0).ok_or_else(|| file_io(path, io::ErrorKind::NotFound.into()))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod platform {
    use std::fs::File;
    use std::os::fd::AsRawFd as _;
    use std::path::{Path, PathBuf};
    use std::sync::Once;

    use tracing::warn;

    use super::beneath::{Beneath, open_beneath, open_walking};
    use super::{WorkspacePath, WorkspaceRoots, file_io, outside, regular_from, require_linked};
    use crate::error::Error;
    use crate::util::RegularFile;

    static PROC_UNAVAILABLE: Once = Once::new();

    /// Logs once that handle paths cannot be read, returning whether it did.
    pub(super) fn note_proc_unavailable(gate: &Once, error: &std::io::Error) -> bool {
        let mut first = false;
        gate.call_once(|| {
            first = true;
            warn!(
                %error,
                "/proc is unavailable; files cannot be verified, so workspace reads fail closed"
            );
        });
        first
    }

    pub(super) fn handle_path_in(fd_dir: &Path, file: &File) -> std::io::Result<PathBuf> {
        std::fs::read_link(fd_dir.join(file.as_raw_fd().to_string()))
    }

    impl WorkspaceRoots {
        pub(super) fn open_contained_with(
            &self,
            path: &WorkspacePath,
            after_open: impl FnOnce(),
        ) -> Result<RegularFile, Error> {
            let mut beneath = self.open_under_root(path, open_beneath)?;
            if matches!(beneath, Beneath::Unsupported) {
                beneath = self.open_under_root(path, open_walking)?;
            }
            let file = regular_from(path, beneath)?;
            after_open();
            let actual =
                handle_path_in(Path::new("/proc/self/fd"), file.file()).map_err(|source| {
                    note_proc_unavailable(&PROC_UNAVAILABLE, &source);
                    file_io(path, source)
                })?;
            require_linked(path, &file)?;
            self.contains_canonical(&actual)
                .ok_or_else(|| outside(path))?;
            Ok(file)
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;

    use super::beneath::open_walking;
    use super::{WorkspacePath, WorkspaceRoots, file_io, outside, regular_from, require_linked};
    use crate::error::Error;
    use crate::util::RegularFile;

    impl WorkspaceRoots {
        pub(super) fn open_contained_with(
            &self,
            path: &WorkspacePath,
            after_open: impl FnOnce(),
        ) -> Result<RegularFile, Error> {
            let beneath = self.open_under_root(path, open_walking)?;
            let file = regular_from(path, beneath)?;
            after_open();
            let raw =
                rustix::fs::getpath(file.file()).map_err(|errno| file_io(path, errno.into()))?;
            let actual = Path::new(std::ffi::OsStr::from_bytes(raw.to_bytes()));
            require_linked(path, &file)?;
            self.contains_canonical(actual)
                .ok_or_else(|| outside(path))?;
            Ok(file)
        }
    }
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_os = "macos"))
))]
mod platform {
    use super::beneath::open_walking;
    use super::{WorkspacePath, WorkspaceRoots, regular_from};
    use crate::error::Error;
    use crate::util::RegularFile;

    impl WorkspaceRoots {
        pub(super) fn open_contained_with(
            &self,
            path: &WorkspacePath,
            after_open: impl FnOnce(),
        ) -> Result<RegularFile, Error> {
            let beneath = self.open_under_root(path, open_walking)?;
            let file = regular_from(path, beneath)?;
            after_open();
            Ok(file)
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::path::Path;

    use super::{WorkspacePath, WorkspaceRoots, file_io, open_regular, outside};
    use crate::error::Error;
    use crate::util::{FileIdentity, RegularFile};

    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_TRAVERSE: u32 = 0x20;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    impl WorkspaceRoots {
        /// Opens every directory from the canonical root down to the parent of
        /// `path` without `FILE_SHARE_DELETE`, so none can be renamed or
        /// replaced while the returned handles live. A name-surrogate reparse
        /// point (symlink or junction) among them is refused.
        pub(super) fn pin_ancestors(&self, path: &WorkspacePath) -> Result<Vec<File>, Error> {
            let file = path.as_path();
            let (root, relative) = self.split_under_root(file).ok_or_else(|| outside(path))?;
            let parent = relative.parent().ok_or_else(|| outside(path))?;
            let mut dir = root.to_path_buf();
            let mut pins = vec![pin(path, &dir)?];
            for component in parent.components() {
                dir.push(component);
                pins.push(pin(path, &dir)?);
            }
            Ok(pins)
        }

        pub(super) fn open_contained_with(
            &self,
            path: &WorkspacePath,
            after_open: impl FnOnce(),
        ) -> Result<RegularFile, Error> {
            let _pins = self.pin_ancestors(path)?;
            let file = open_regular(path)?;
            after_open();
            let unfollowed = OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(path.as_path())
                .map_err(|source| file_io(path, source))?;
            let swapped = unfollowed
                .metadata()
                .map_err(|source| file_io(path, source))?
                .file_type()
                .is_symlink();
            let same = FileIdentity::of_file(file.file())
                .map_err(|source| file_io(path, source))?
                == FileIdentity::of_file(&unfollowed).map_err(|source| file_io(path, source))?;
            if swapped || !same {
                return Err(outside(path));
            }
            Ok(file)
        }
    }

    fn pin(path: &WorkspacePath, dir: &Path) -> Result<File, Error> {
        let handle = OpenOptions::new()
            .access_mode(FILE_TRAVERSE | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(dir)
            .map_err(|source| Error::FileIo {
                path: dir.to_path_buf(),
                source,
            })?;
        let is_link = handle
            .metadata()
            .map_err(|source| Error::FileIo {
                path: dir.to_path_buf(),
                source,
            })?
            .file_type()
            .is_symlink();
        if is_link {
            Err(outside(path))
        } else {
            Ok(handle)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    #[cfg(unix)]
    use std::path::Path;

    use super::*;
    use crate::bridge::ClientPath;

    fn workspace() -> (tempfile::TempDir, std::path::PathBuf, WorkspaceRoots) {
        let dir = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(dir.path()).unwrap();
        let root = base.join("ws");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(base.join("out")).unwrap();
        let roots = WorkspaceRoots::from_paths(&[&root]).unwrap();
        (dir, base, roots)
    }

    fn validated(roots: &WorkspaceRoots, path: std::path::PathBuf) -> WorkspacePath {
        roots
            .validate_blocking(&ClientPath::try_from(path).unwrap())
            .unwrap()
    }

    #[test]
    fn test_open_contained_opens_file_inside_roots() {
        let (_guard, base, roots) = workspace();
        let file = base.join("ws/sub/a.rs");
        std::fs::write(&file, "fn a() {}").unwrap();
        let path = validated(&roots, file);

        let opened = roots.open_contained(&path).unwrap();

        assert_eq!(opened.file().metadata().unwrap().len(), 9);
    }

    #[test]
    fn test_open_contained_missing_file_stays_file_io() {
        let (_guard, base, roots) = workspace();
        let file = base.join("ws/sub/a.rs");
        std::fs::write(&file, "x").unwrap();
        let path = validated(&roots, file.clone());
        std::fs::remove_file(&file).unwrap();

        assert_matches!(roots.open_contained(&path), Err(Error::FileIo { .. }));
    }

    #[test]
    fn test_open_contained_directory_is_not_a_regular_file() {
        let (_guard, base, roots) = workspace();
        let path = validated(&roots, base.join("ws/sub"));

        assert_matches!(roots.open_contained(&path), Err(Error::NotARegularFile(_)));
    }

    #[cfg(unix)]
    #[test]
    fn test_open_contained_refuses_parent_swapped_for_outside_symlink() {
        let (_guard, base, roots) = workspace();
        let file = base.join("ws/sub/a.rs");
        std::fs::write(&file, "inside").unwrap();
        let path = validated(&roots, file);
        std::fs::write(base.join("out/a.rs"), "secret").unwrap();
        std::fs::remove_dir_all(base.join("ws/sub")).unwrap();
        std::os::unix::fs::symlink(base.join("out"), base.join("ws/sub")).unwrap();

        assert_matches!(
            roots.open_contained(&path),
            Err(Error::PathOutsideWorkspace(_))
        );
    }

    #[cfg(unix)]
    fn outside_fifo_behind_symlink(
        base: &std::path::Path,
        roots: &WorkspaceRoots,
    ) -> WorkspacePath {
        let file = base.join("ws/sub/a.rs");
        std::fs::write(&file, "inside").unwrap();
        let path = validated(roots, file.clone());
        let fifo = base.join("out/pipe");
        crate::test_lsp::make_fifo(&fifo);
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&fifo, &file).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn test_open_contained_refuses_symlink_to_outside_fifo() {
        let (_guard, base, roots) = workspace();
        let path = outside_fifo_behind_symlink(&base, &roots);

        assert_matches!(
            roots.open_contained(&path),
            Err(Error::PathOutsideWorkspace(_))
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_walking_open_never_opens_a_symlinked_outside_target() {
        use super::beneath::{Beneath, open_walking};

        let (_guard, base, roots) = workspace();
        let _path = outside_fifo_behind_symlink(&base, &roots);

        let outcome = open_walking(&base.join("ws"), Path::new("sub/a.rs")).unwrap();

        assert_matches!(outcome, Beneath::Escaped);
    }

    #[cfg(unix)]
    #[test]
    fn test_walking_open_refuses_symlinked_directory_component() {
        use super::beneath::{Beneath, open_walking};

        let (_guard, base, _roots) = workspace();
        std::fs::write(base.join("out/a.rs"), "secret").unwrap();
        std::fs::remove_dir_all(base.join("ws/sub")).unwrap();
        std::os::unix::fs::symlink(base.join("out"), base.join("ws/sub")).unwrap();

        let outcome = open_walking(&base.join("ws"), Path::new("sub/a.rs")).unwrap();

        assert_matches!(outcome, Beneath::Escaped);
    }

    #[cfg(unix)]
    #[test]
    fn test_walking_open_opens_regular_file_inside_root() {
        use super::beneath::{Beneath, open_walking};

        let (_guard, base, _roots) = workspace();
        std::fs::write(base.join("ws/sub/a.rs"), "inside").unwrap();

        let outcome = open_walking(&base.join("ws"), Path::new("sub/a.rs")).unwrap();

        let Beneath::Opened(file) = outcome else {
            panic!("expected an opened file, got {outcome:?}");
        };
        assert_eq!(file.metadata().unwrap().len(), 6);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn test_openat2_refuses_outside_symlink_without_opening_it() {
        use super::beneath::{Beneath, open_beneath};

        let (_guard, base, roots) = workspace();
        let _path = outside_fifo_behind_symlink(&base, &roots);

        let outcome = open_beneath(&base.join("ws"), Path::new("sub/a.rs")).unwrap();

        match outcome {
            Beneath::Escaped => {}
            // The sandbox running the tests blocks openat2; nothing to assert
            Beneath::Unsupported => eprintln!("openat2 unavailable, test skipped"),
            Beneath::Opened(_) => panic!("openat2 opened an outside target"),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn test_open_contained_refuses_directory_moved_out_after_the_open() {
        let (_guard, base, roots) = workspace();
        let file = base.join("ws/sub/a.rs");
        std::fs::write(&file, "inside").unwrap();
        let path = validated(&roots, file);

        let result = roots.open_contained_with(&path, || {
            std::fs::rename(base.join("ws/sub"), base.join("out/sub")).unwrap();
        });

        assert_matches!(result, Err(Error::PathOutsideWorkspace(_)));
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn test_open_contained_file_deleted_after_the_open_is_file_io() {
        let (_guard, base, roots) = workspace();
        let file = base.join("ws/sub/a.rs");
        std::fs::write(&file, "inside").unwrap();
        let path = validated(&roots, file.clone());

        let result = roots.open_contained_with(&path, || std::fs::remove_file(&file).unwrap());

        assert_matches!(result, Err(Error::FileIo { .. }));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_open_contained_passes_after_open_through_another_case_spelling() {
        let (_guard, base, roots) = workspace();
        let file = base.join("ws/sub/a.rs");
        std::fs::write(&file, "inside").unwrap();
        let path = validated(&roots, file);
        let shouted = base.join("WS/SUB/A.RS");
        let _poison = std::fs::File::open(shouted);

        assert!(roots.open_contained(&path).is_ok());
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn test_unreadable_proc_is_logged_once_and_fails_as_file_io() {
        use std::sync::Once;

        let (_guard, base, roots) = workspace();
        let file = base.join("ws/sub/a.rs");
        std::fs::write(&file, "x").unwrap();
        let path = validated(&roots, file);
        let opened = open_regular(&path).unwrap();
        let error = platform::handle_path_in(&base.join("no-proc"), opened.file()).unwrap_err();

        let gate = Once::new();
        assert!(platform::note_proc_unavailable(&gate, &error));
        assert!(!platform::note_proc_unavailable(&gate, &error));
    }

    #[cfg(windows)]
    #[test]
    fn test_pinned_ancestor_cannot_be_renamed() {
        let (_guard, base, roots) = workspace();
        let file = base.join("ws/sub/a.rs");
        std::fs::write(&file, "inside").unwrap();
        let path = validated(&roots, file);

        let pins = roots.pin_ancestors(&path).unwrap();
        let renamed = std::fs::rename(base.join("ws/sub"), base.join("ws/moved"));

        let code = renamed.unwrap_err().raw_os_error();
        assert_matches!(code, Some(32 | 5));
        drop(pins);
        std::fs::rename(base.join("ws/sub"), base.join("ws/moved")).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn test_pin_sharing_violation_is_file_io_naming_the_ancestor() {
        use std::os::windows::fs::OpenOptionsExt as _;

        let (_guard, base, roots) = workspace();
        let file = base.join("ws/sub/a.rs");
        std::fs::write(&file, "inside").unwrap();
        let path = validated(&roots, file);
        let exclusive = std::fs::OpenOptions::new()
            .access_mode(0x20)
            .share_mode(0)
            .custom_flags(0x0200_0000)
            .open(base.join("ws/sub"))
            .unwrap();

        let error = roots.pin_ancestors(&path).unwrap_err();

        assert_matches!(error, Error::FileIo { path, .. } if path == base.join("ws/sub"));
        drop(exclusive);
    }

    #[cfg(windows)]
    #[test]
    fn test_open_contained_refuses_parent_swapped_for_outside_junction() {
        let (_guard, base, roots) = workspace();
        let file = base.join("ws/sub/a.rs");
        std::fs::write(&file, "inside").unwrap();
        let path = validated(&roots, file);
        std::fs::write(base.join("out/a.rs"), "secret").unwrap();
        std::fs::remove_dir_all(base.join("ws/sub")).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(base.join("ws").join("sub"))
            .arg(base.join("out"))
            .status()
            .unwrap();
        assert!(status.success());

        assert_matches!(
            roots.open_contained(&path),
            Err(Error::PathOutsideWorkspace(_))
        );
    }
}
