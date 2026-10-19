//! The argument backstop of untrusted-workspace mode.
//!
//! No launcher table is complete, so an argument that names an executable
//! file inside the workspace is refused whichever program receives it. The
//! planner runs the check when it admits a server, and [`super::LspServer`]
//! runs it again right before every spawn, because a workspace file can become
//! executable after planning.

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::bridge::{WorkspaceRoots, lexically_normalize};
use crate::config::HostOs;
use crate::error::UntrustedRefusal;
use crate::lsp::ChildWorkingDir;

/// The directories no executable or working directory of an untrusted server
/// may lie in, kept with the server so the argument backstop can run again at
/// spawn time.
#[derive(Debug, Clone)]
pub struct UntrustedBoundary(WorkspaceRoots);

impl UntrustedBoundary {
    /// The boundary `roots` draws.
    pub const fn new(roots: WorkspaceRoots) -> Self {
        Self(roots)
    }

    /// Refuses an argument of `args` that names an executable file inside the
    /// boundary, as [`refuse_workspace_executable_arguments`] does.
    ///
    /// Walks the file system, so async callers run it on the blocking pool.
    pub fn refuse_arguments(
        &self,
        args: &[String],
        working_dir: &ChildWorkingDir,
        host: HostOs,
    ) -> Result<(), UntrustedRefusal> {
        refuse_workspace_executable_arguments(args, &self.0, working_dir, host)
    }
}

/// Extensions Windows starts as programs.
const WINDOWS_EXECUTABLE_EXTENSIONS: [&str; 4] = ["exe", "com", "bat", "cmd"];

/// Whether `path` is a file `host` would start: an execute bit on Unix, and on
/// Windows an extension it starts as a program.
pub fn is_started_as_program(host: HostOs, path: &Path) -> bool {
    host.is_executable_file(path)
        && (host == HostOs::Other
            || path
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| {
                    WINDOWS_EXECUTABLE_EXTENSIONS
                        .iter()
                        .any(|known| extension.eq_ignore_ascii_case(known))
                }))
}

/// The refusal for the text `candidate` of argument `index`, when it names an
/// executable inside `boundary` as the child would resolve it.
///
/// A lookup that fails with permission denied fails closed when the path lies
/// inside the boundary; any other failure means the text is not a file.
fn argument_refusal(
    index: usize,
    candidate: &str,
    boundary: &WorkspaceRoots,
    working_dir: &ChildWorkingDir,
    host: HostOs,
) -> Option<UntrustedRefusal> {
    let resolved = match working_dir {
        ChildWorkingDir::Fixed(dir) => dir.join(candidate),
        ChildWorkingDir::Inherit => PathBuf::from(candidate),
    };
    let siblings = (host == HostOs::Windows && resolved.extension().is_none())
        .then(|| {
            WINDOWS_EXECUTABLE_EXTENSIONS
                .iter()
                .map(|extension| resolved.with_added_extension(extension))
        })
        .into_iter()
        .flatten();
    std::iter::once(resolved.clone())
        .chain(siblings)
        .find_map(|path| path_refusal(index, &path, boundary, host))
}

/// The refusal for `resolved`, one spelling of argument `index`.
fn path_refusal(
    index: usize,
    resolved: &Path,
    boundary: &WorkspaceRoots,
    host: HostOs,
) -> Option<UntrustedRefusal> {
    match dunce::canonicalize(resolved) {
        Ok(canonical) => (is_started_as_program(host, &canonical)
            && boundary.contains_canonical(&canonical))
        .then_some(UntrustedRefusal::WorkspaceExecutableArgument {
            index,
            executable: canonical,
        }),
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            let path = lexically_normalize(resolved);
            boundary
                .admits_lexically(&path)
                .then_some(UntrustedRefusal::UnreadableWorkspaceArgument { index, path })
        }
        Err(_) => None,
    }
}

/// Refuses a configured argument that names an executable file inside
/// `boundary`, whichever launcher precedes it, so a wrapper missing from the
/// launcher table cannot start a workspace program.
///
/// Each argument is judged as a path, and so is the text after its first `=`
/// (`--exec=<path>`). A file that is not executable, a directory and text that
/// is no file are admitted.
pub fn refuse_workspace_executable_arguments(
    args: &[String],
    boundary: &WorkspaceRoots,
    working_dir: &ChildWorkingDir,
    host: HostOs,
) -> Result<(), UntrustedRefusal> {
    for (index, arg) in args.iter().enumerate() {
        let after_equals = arg.split_once('=').map(|(_, value)| value);
        let candidates = std::iter::once(arg.as_str()).chain(after_equals);
        for candidate in candidates.filter(|candidate| !candidate.is_empty()) {
            if let Some(refusal) = argument_refusal(index, candidate, boundary, working_dir, host) {
                return Err(refusal);
            }
        }
    }
    Ok(())
}
