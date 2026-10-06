//! The file stem of a command, compared the one way every launcher rule uses.

use std::path::Path;

/// The file stem of a command (`/usr/bin/NPX.cmd` is `npx`), lowercased.
///
/// One constructor and one case rule serve the launcher rules, the TypeScript
/// pin and builtin-server matching. The stem is lowercased once, so a
/// comparison is case-insensitive on every host: on a case-insensitive file
/// system (Windows, and macOS by default) a differently spelled command runs
/// the same program, so a case-sensitive denylist (launchers) would let one
/// slip through. For allow-style matches (the TypeScript pin, builtin-server
/// matching) the same rule is merely generous on a case-sensitive file system:
/// a differently cased name there fails to start rather than running anything
/// unvetted. The names compared against must be lowercase.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::CommandStem;
///
/// let stem = CommandStem::of("/usr/local/bin/NPX.cmd");
/// assert!(stem.is("npx"));
/// assert!(stem.is_any(&["yarn", "npx"]));
/// assert_eq!(stem.as_str(), "npx");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandStem(String);

impl CommandStem {
    /// The stem of `command`'s final path component; empty when it has none.
    #[must_use]
    pub fn of(command: &str) -> Self {
        Self(
            Path::new(command)
                .file_stem()
                .map_or_default(|stem| stem.to_string_lossy().to_ascii_lowercase()),
        )
    }

    /// The lowercased stem.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the stem is `name`, a lowercase name.
    #[must_use]
    pub fn is(&self, name: &str) -> bool {
        debug_assert!(
            name.bytes().all(|byte| !byte.is_ascii_uppercase()),
            "{name} must be lowercase"
        );
        self.0 == name
    }

    /// Whether the stem is any of `names`, lowercase names.
    #[must_use]
    pub fn is_any(&self, names: &[&str]) -> bool {
        names.iter().any(|name| self.is(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_drops_directory_and_extension_and_lowercases() {
        for command in ["npx", "/a/b/NPX", "/a/Npx.CMD", "npx.cmd"] {
            assert_eq!(CommandStem::of(command).as_str(), "npx", "{command}");
        }
        assert_eq!(CommandStem::of("").as_str(), "");
        assert_eq!(CommandStem::of("/").as_str(), "");
    }

    #[test]
    fn is_any_matches_any_listed_name_regardless_of_case() {
        let stem = CommandStem::of("Bunx.exe");
        assert!(stem.is_any(&["npx", "bunx"]));
        assert!(!stem.is_any(&["npx", "pnpx"]));
        assert!(!stem.is_any(&[]));
    }
}
