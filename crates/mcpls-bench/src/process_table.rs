//! Snapshots of the system process table and the process trees inside them.
//!
//! A process is identified by its pid together with its start time, never by
//! the pid alone: pids are recycled, and a snapshot is always older than the
//! decision taken on it.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};

use crate::report::Kib;

/// Start time of a process as the platform prints it; equal only for the same process incarnation.
///
/// On Unix it is the five-field `lstart` text. On Windows it is the ISO 8601
/// round-trip form of `CreationDate`, which orders chronologically as a string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StartTime(String);

impl StartTime {
    /// Wraps the platform's start time text.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The start time text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A process incarnation: a pid together with the time it started.
///
/// # Examples
///
/// ```
/// use mcpls_bench::process_table::{ProcessIdentity, StartTime};
///
/// let first = ProcessIdentity::new(7, StartTime::new("Sun Oct  4 16:49:47 2026"));
/// let recycled = ProcessIdentity::new(7, StartTime::new("Sun Oct  4 16:51:02 2026"));
/// assert_ne!(first, recycled);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessIdentity {
    pid: u32,
    started: StartTime,
}

impl ProcessIdentity {
    /// Combines a pid with its start time.
    #[must_use]
    pub const fn new(pid: u32, started: StartTime) -> Self {
        Self { pid, started }
    }

    /// The process id.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// The start time.
    #[must_use]
    pub const fn started(&self) -> &StartTime {
        &self.started
    }
}

/// One process of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRow {
    /// Pid and start time.
    pub identity: ProcessIdentity,
    /// Parent pid as recorded when the process started; stale after the parent exits on Windows.
    pub ppid: u32,
    /// Process group id; `None` where the platform has no groups.
    pub pgid: Option<u32>,
    /// Exited but not yet reaped.
    pub zombie: bool,
    /// Resident set size.
    pub rss: Kib,
    /// Executable name.
    pub command: String,
}

/// When a parent pid links a process to the tree below that parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentLink {
    /// The pid alone: a live process holds its parent's pid.
    Pid,
    /// The parent must also have started before the child: Windows keeps the
    /// parent pid after the parent exits, and recycles pids aggressively.
    OlderParent,
}

impl ParentLink {
    /// The rule for the platform this binary runs on.
    pub const PLATFORM: Self = if cfg!(windows) {
        Self::OlderParent
    } else {
        Self::Pid
    };

    fn accepts(self, parent: &ProcessRow, child: &ProcessRow) -> bool {
        match self {
            Self::Pid => true,
            Self::OlderParent => parent.identity.started < child.identity.started,
        }
    }
}

/// A snapshot of all processes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessTable {
    rows: Vec<ProcessRow>,
}

const LSTART_FIELDS: usize = 5;

fn parse_unix_row(line: &str) -> Option<ProcessRow> {
    let mut fields = line.split_ascii_whitespace();
    let id = fields.next()?.parse().ok()?;
    let parent = fields.next()?.parse().ok()?;
    let group = fields.next()?.parse().ok()?;
    let zombie = fields.next()?.starts_with('Z');
    let rss = fields.next()?.parse().ok()?;
    let started: Vec<&str> = fields.by_ref().take(LSTART_FIELDS).collect();
    if started.len() < LSTART_FIELDS {
        return None;
    }
    let command = fields.collect::<Vec<_>>().join(" ");
    if command.is_empty() {
        return None;
    }
    Some(ProcessRow {
        identity: ProcessIdentity::new(id, StartTime::new(started.join(" "))),
        ppid: parent,
        pgid: Some(group),
        zombie,
        rss: Kib(rss),
        command,
    })
}

fn split_csv(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    fields.push(current);
    fields
}

fn parse_windows_row(line: &str) -> Option<ProcessRow> {
    let fields = split_csv(line.trim());
    let [id, parent, working_set, created, name] = fields.as_slice() else {
        return None;
    };
    if created.is_empty() {
        return None;
    }
    Some(ProcessRow {
        identity: ProcessIdentity::new(id.parse().ok()?, StartTime::new(created.as_str())),
        ppid: parent.parse().ok()?,
        pgid: None,
        zombie: false,
        rss: Kib(working_set.parse::<u64>().unwrap_or(0) / 1024),
        command: name.clone(),
    })
}

impl ProcessTable {
    /// Builds a table from rows.
    #[must_use]
    pub const fn from_rows(rows: Vec<ProcessRow>) -> Self {
        Self { rows }
    }

    /// Parses `ps -A -o pid=,ppid=,pgid=,stat=,rss=,lstart=,comm=` output.
    ///
    /// `comm` is the last column because it may contain spaces on macOS.
    /// Unparsable lines are skipped, so one odd process cannot hide the rest.
    ///
    /// # Errors
    ///
    /// Returns an error when non-empty output contains no parsable line, which means
    /// the `ps` flavour prints something else.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_bench::process_table::ProcessTable;
    ///
    /// let table = ProcessTable::parse_unix(
    ///     "10 1 10 Ss 2048 Sun Oct  4 16:49:47 2026 mcpls\n11 10 10 S 4096 Sun Oct  4 16:49:48 2026 rust-analyzer\n",
    /// )
    /// .unwrap();
    /// assert_eq!(table.rows().len(), 2);
    /// ```
    pub fn parse_unix(output: &str) -> Result<Self> {
        Self::parse_with(output, parse_unix_row, "ps")
    }

    /// Parses the CSV of `Get-CimInstance Win32_Process` with the columns
    /// `ProcessId, ParentProcessId, WorkingSetSize, Created, Name`.
    ///
    /// # Errors
    ///
    /// Returns an error when non-empty output contains no parsable row.
    pub fn parse_windows(output: &str) -> Result<Self> {
        Self::parse_with(output, parse_windows_row, "process listing")
    }

    fn parse_with(
        output: &str,
        parse_row: fn(&str) -> Option<ProcessRow>,
        source: &str,
    ) -> Result<Self> {
        let mut seen_text = false;
        let mut rows = Vec::new();
        for line in output.lines().filter(|line| !line.trim().is_empty()) {
            seen_text = true;
            rows.extend(parse_row(line));
        }
        if seen_text && rows.is_empty() {
            bail!("no line of the {source} output could be parsed");
        }
        Ok(Self { rows })
    }

    /// Every process of the snapshot.
    #[must_use]
    pub fn rows(&self) -> &[ProcessRow] {
        &self.rows
    }

    /// The live (not zombie) process with exactly this identity.
    #[must_use]
    pub fn find(&self, identity: &ProcessIdentity) -> Option<&ProcessRow> {
        self.rows
            .iter()
            .find(|row| !row.zombie && &row.identity == identity)
    }

    /// The identity of the live process holding `pid`.
    #[must_use]
    pub fn identity_of(&self, pid: u32) -> Option<&ProcessIdentity> {
        self.rows
            .iter()
            .find(|row| !row.zombie && row.identity.pid() == pid)
            .map(|row| &row.identity)
    }

    /// Live members of process group `pgid`.
    pub fn group_members(&self, pgid: u32) -> impl Iterator<Item = &ProcessRow> {
        self.rows
            .iter()
            .filter(move |row| !row.zombie && row.pgid == Some(pgid))
    }

    /// `root` and every live descendant reachable through parent links, `root` first.
    ///
    /// Empty when `root` is not alive with exactly this identity, so a recycled pid
    /// never yields a foreign tree.
    #[must_use]
    pub fn tree_of(&self, root: &ProcessIdentity, link: ParentLink) -> Vec<&ProcessRow> {
        let Some(root_row) = self.find(root) else {
            return Vec::new();
        };
        let mut children: HashMap<u32, Vec<&ProcessRow>> = HashMap::new();
        for row in self.rows.iter().filter(|row| !row.zombie) {
            children.entry(row.ppid).or_default().push(row);
        }
        let mut seen = HashSet::from([root_row.identity.pid()]);
        let mut tree = vec![root_row];
        let mut next = 0;
        while let Some(&parent) = tree.get(next) {
            next += 1;
            for &child in children.get(&parent.identity.pid()).into_iter().flatten() {
                if link.accepts(parent, child) && seen.insert(child.identity.pid()) {
                    tree.push(child);
                }
            }
        }
        tree
    }
}

/// The pid as an `i32` when it may be signalled: 0 and 1 address the caller's group
/// or init, and ids beyond `i32` are not valid pids.
#[must_use]
pub fn signalable_pid(pid: u32) -> Option<i32> {
    i32::try_from(pid).ok().filter(|&p| p > 1)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const T0: &str = "Sun Oct  4 16:00:00 2026";
    const T1: &str = "Sun Oct  4 16:00:05 2026";

    fn row(id: u32, parent: u32, group: u32, started: &str, command: &str) -> ProcessRow {
        ProcessRow {
            identity: ProcessIdentity::new(id, StartTime::new(started)),
            ppid: parent,
            pgid: Some(group),
            zombie: false,
            rss: Kib(u64::from(id) * 10),
            command: command.to_owned(),
        }
    }

    fn pids(tree: &[&ProcessRow]) -> Vec<u32> {
        tree.iter().map(|row| row.identity.pid()).collect()
    }

    #[test]
    fn unix_rows_are_parsed_with_spaced_commands_and_zombies() {
        let output = "  500     1   500 Ss     2048 Sun Oct  4 16:00:00 2026 mcpls\n\
                      501   500   501 S     40960 Sun Oct  4 16:00:01 2026 Google Chrome Helper\n\
                      502   500   500 Z         0 Sun Oct  4 16:00:02 2026 <defunct>\n";
        let table = ProcessTable::parse_unix(output).unwrap();
        assert_eq!(table.rows().len(), 3);
        assert_eq!(table.rows()[1].command, "Google Chrome Helper");
        assert_eq!(
            table.rows()[1].identity.started().as_str(),
            "Sun Oct 4 16:00:01 2026"
        );
        assert!(table.rows()[2].zombie);
        assert_eq!(table.rows()[0].rss, Kib(2048));
    }

    #[test]
    fn odd_lines_are_skipped_but_unrecognisable_output_is_an_error() {
        let output = "garbage\n7 1 7 S 5 Sun Oct  4 16:00:00 2026 ok\n x\n";
        assert_eq!(ProcessTable::parse_unix(output).unwrap().rows().len(), 1);
        assert!(ProcessTable::parse_unix("nothing useful here\n").is_err());
        assert!(ProcessTable::parse_unix("").unwrap().rows().is_empty());
    }

    #[test]
    fn windows_rows_are_parsed_from_csv() {
        let output = "\"ProcessId\",\"ParentProcessId\",\"WorkingSetSize\",\"Created\",\"Name\"\r\n\
                      \"0\",\"0\",\"8192\",\"\",\"System Idle Process\"\r\n\
                      \"812\",\"4\",\"10485760\",\"2026-10-04T16:00:00.1234567+03:00\",\"mcpls.exe\"\r\n";
        let table = ProcessTable::parse_windows(output).unwrap();
        assert_eq!(table.rows().len(), 1);
        let mcpls = &table.rows()[0];
        assert_eq!((mcpls.identity.pid(), mcpls.ppid), (812, 4));
        assert_eq!(mcpls.rss, Kib(10_240));
        assert_eq!(mcpls.pgid, None);
        assert_eq!(split_csv("\"a,b\",\"c\"\"d\",e"), ["a,b", "c\"d", "e"]);
    }

    #[test]
    fn tree_follows_parent_links_and_ignores_unrelated_processes() {
        let table = ProcessTable::from_rows(vec![
            row(10, 1, 10, T0, "mcpls"),
            row(11, 10, 11, T0, "watchdog"),
            row(12, 11, 11, T0, "rust-analyzer"),
            row(13, 12, 13, T1, "flycheck"),
            row(20, 1, 20, T0, "unrelated"),
        ]);
        let root = ProcessIdentity::new(10, StartTime::new(T0));
        let tree = table.tree_of(&root, ParentLink::Pid);
        assert_eq!(pids(&tree), [10, 11, 12, 13]);
    }

    #[test]
    fn a_recycled_root_pid_yields_no_tree() {
        let table = ProcessTable::from_rows(vec![
            row(10, 1, 10, T1, "stranger"),
            row(11, 10, 10, T1, "child"),
        ]);
        let stale = ProcessIdentity::new(10, StartTime::new(T0));
        assert!(table.tree_of(&stale, ParentLink::Pid).is_empty());
    }

    #[test]
    fn zombies_are_not_part_of_a_tree() {
        let mut zombie = row(12, 10, 10, T0, "gone");
        zombie.zombie = true;
        let table = ProcessTable::from_rows(vec![row(10, 1, 10, T0, "mcpls"), zombie]);
        let root = ProcessIdentity::new(10, StartTime::new(T0));
        assert_eq!(pids(&table.tree_of(&root, ParentLink::Pid)), [10]);
    }

    #[test]
    fn windows_links_require_an_older_parent() {
        let table = ProcessTable::from_rows(vec![
            row(10, 4, 10, "2026-10-04T16:00:05", "mcpls"),
            row(11, 10, 11, "2026-10-04T16:00:06", "child"),
            row(
                12,
                10,
                12,
                "2026-10-04T15:00:00",
                "foreign-older-than-parent",
            ),
        ]);
        let root = ProcessIdentity::new(10, StartTime::new("2026-10-04T16:00:05"));
        assert_eq!(
            pids(&table.tree_of(&root, ParentLink::OlderParent)),
            [10, 11]
        );
        assert_eq!(pids(&table.tree_of(&root, ParentLink::Pid)), [10, 11, 12]);
    }

    #[test]
    fn cyclic_parent_links_terminate() {
        let table =
            ProcessTable::from_rows(vec![row(10, 11, 10, T0, "a"), row(11, 10, 10, T0, "b")]);
        let root = ProcessIdentity::new(10, StartTime::new(T0));
        assert_eq!(pids(&table.tree_of(&root, ParentLink::Pid)), [10, 11]);
    }

    #[test]
    fn identity_requires_the_start_time_to_match() {
        let table = ProcessTable::from_rows(vec![row(10, 1, 10, T1, "new")]);
        assert!(
            table
                .find(&ProcessIdentity::new(10, StartTime::new(T0)))
                .is_none()
        );
        assert!(
            table
                .find(&ProcessIdentity::new(10, StartTime::new(T1)))
                .is_some()
        );
        assert_eq!(table.identity_of(10).unwrap().started().as_str(), T1);
    }

    #[test]
    fn group_members_exclude_zombies() {
        let mut zombie = row(12, 10, 10, T0, "gone");
        zombie.zombie = true;
        let table = ProcessTable::from_rows(vec![
            row(10, 1, 10, T0, "a"),
            row(11, 1, 11, T0, "b"),
            zombie,
        ]);
        assert_eq!(table.group_members(10).count(), 1);
    }

    #[test]
    fn init_and_oversized_pids_are_not_signalable() {
        assert_eq!(signalable_pid(0), None);
        assert_eq!(signalable_pid(1), None);
        assert_eq!(signalable_pid(2), Some(2));
        assert_eq!(signalable_pid(u32::MAX), None);
    }
}
