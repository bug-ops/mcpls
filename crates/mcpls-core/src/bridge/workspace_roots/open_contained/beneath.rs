//! Opening a file strictly beneath a root directory handle.
//!
//! Both strategies resolve the path relative to an open directory, so a
//! component swapped for a symlink cannot lead outside the root and an outside
//! target (a device node, a FIFO) is never opened.
//!
//! - Linux and Android use `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`
//!   (kernel 5.6 or newer); [`Beneath::Unsupported`] reports a kernel or
//!   sandbox without it.
//! - Every other Unix system walks the components with `O_NOFOLLOW`. A
//!   validated path is canonical, so a symlink among its components means the
//!   tree changed after validation.

use std::fs::File;
use std::io;
use std::path::Path;

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

/// Outcome of a containment-aware open.
#[derive(Debug)]
pub(super) enum Beneath {
    /// The file was opened under the root.
    Opened(File),
    /// The path leads outside the root (or through a symlink, for the walk).
    Escaped,
    /// The kernel or its sandbox does not offer the syscall; use a fallback.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    Unsupported,
}

fn file_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC
}

/// Flags for a directory handle used only to resolve names: `O_PATH` where it
/// exists, so a directory with search but no read permission still works.
fn dir_flags() -> OFlags {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    let access = OFlags::PATH;
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    let access = OFlags::RDONLY;
    access | OFlags::DIRECTORY | OFlags::CLOEXEC
}

/// Opens `relative` under `root` with `openat2`, refusing anything that
/// resolves outside `root`.
///
/// # Errors
///
/// The underlying I/O error, including when the root cannot be opened.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn open_beneath(root: &Path, relative: &Path) -> io::Result<Beneath> {
    use rustix::fs::{ResolveFlags, openat2};

    const ATTEMPTS: usize = 4;

    let root = rustix::fs::open(root, dir_flags(), Mode::empty())?;
    let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS;
    // EAGAIN is the kernel's spurious refusal when it raced a rename
    for _ in 0..ATTEMPTS {
        match openat2(&root, relative, file_flags(), Mode::empty(), resolve) {
            Ok(fd) => return Ok(Beneath::Opened(File::from(fd))),
            Err(Errno::NOSYS | Errno::PERM) => return Ok(Beneath::Unsupported),
            Err(Errno::XDEV | Errno::LOOP) => return Ok(Beneath::Escaped),
            Err(Errno::AGAIN) => {}
            Err(errno) => return Err(errno.into()),
        }
    }
    Err(Errno::AGAIN.into())
}

/// Opens `relative` under `root` one component at a time, refusing a symlink
/// at any step.
///
/// # Errors
///
/// The underlying I/O error, including when the root cannot be opened.
#[cfg(all(unix, any(not(target_os = "macos"), test)))]
pub(super) fn open_walking(root: &Path, relative: &Path) -> io::Result<Beneath> {
    use std::path::Component;

    let mut dir = rustix::fs::open(root, dir_flags(), Mode::empty())?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Ok(Beneath::Escaped);
        };
        let last = components.peek().is_none();
        let flags = if last {
            file_flags() | OFlags::NOFOLLOW
        } else {
            dir_flags() | OFlags::NOFOLLOW
        };
        match rustix::fs::openat(&dir, name, flags, Mode::empty()) {
            Ok(fd) if last => return Ok(Beneath::Opened(File::from(fd))),
            Ok(fd) => dir = fd,
            Err(errno) if is_symlink_refusal(errno) => return Ok(Beneath::Escaped),
            Err(errno) => return Err(errno.into()),
        }
    }
    Ok(Beneath::Escaped)
}

/// Errors `O_NOFOLLOW` (with `O_DIRECTORY` for a directory step) gives for a
/// symlink: `ELOOP`, `EMLINK` (FreeBSD), `EFTYPE` (NetBSD) or `ENOTDIR`
/// (macOS). `ENOTDIR` also covers a regular file in place of a directory, which
/// is treated the same: the tree no longer matches the validated path.
#[cfg(all(unix, any(not(target_os = "macos"), test)))]
const fn is_symlink_refusal(errno: Errno) -> bool {
    #[cfg(target_os = "netbsd")]
    if matches!(errno, Errno::FTYPE) {
        return true;
    }
    matches!(errno, Errno::LOOP | Errno::MLINK | Errno::NOTDIR)
}
