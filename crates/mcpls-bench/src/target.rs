//! What is being measured: mcpls, or a comparison MCP server driven by the same scenario.
//!
//! A comparison target brings its own language servers, so only the probes and
//! the repository come from the scenario. Its tools are bound to probe kinds in
//! a TOML definition whose argument placeholders are parsed when it loads.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject};
use serde::Deserialize;
use serde_json::Value;

use crate::probe::Incorrect;
use crate::report::Verification;
use crate::scenario::{Executable, Probe, RepoPath, is_slug};

/// A value a scenario supplies to a tool argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placeholder {
    /// The repository root, absolute.
    Repo,
    /// The probe's file, absolute.
    File,
    /// The probe's file relative to the repository root.
    RelativeFile,
    /// The probe's 1-based line.
    Line1,
    /// The probe's 0-based line.
    Line0,
    /// The probe's 1-based character.
    Character1,
    /// The probe's 0-based character.
    Character0,
    /// The identifier the probe is about.
    Symbol,
}

impl Placeholder {
    const ALL: [Self; 8] = [
        Self::Repo,
        Self::File,
        Self::RelativeFile,
        Self::Line1,
        Self::Line0,
        Self::Character1,
        Self::Character0,
        Self::Symbol,
    ];

    /// The `{name}` form used in definition files.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Repo => "{repo}",
            Self::File => "{file}",
            Self::RelativeFile => "{relative_file}",
            Self::Line1 => "{line1}",
            Self::Line0 => "{line0}",
            Self::Character1 => "{character1}",
            Self::Character0 => "{character0}",
            Self::Symbol => "{symbol}",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.token() == text)
    }
}

/// One tool argument: a constant, or a placeholder filled per probe.
///
/// # Examples
///
/// ```
/// use mcpls_bench::target::{ArgValue, Placeholder};
///
/// assert_eq!(
///     ArgValue::try_from(toml::Value::String("{line0}".to_owned())).unwrap(),
///     ArgValue::Placeholder(Placeholder::Line0)
/// );
/// assert!(ArgValue::try_from(toml::Value::String("{nope}".to_owned())).is_err());
/// assert!(ArgValue::try_from(toml::Value::String("pre{file}".to_owned())).is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "toml::Value")]
pub enum ArgValue {
    /// A constant string.
    Text(String),
    /// A constant integer.
    Number(i64),
    /// A constant boolean.
    Flag(bool),
    /// Filled from the probe.
    Placeholder(Placeholder),
}

impl TryFrom<toml::Value> for ArgValue {
    type Error = String;

    fn try_from(value: toml::Value) -> Result<Self, Self::Error> {
        match value {
            toml::Value::Boolean(flag) => Ok(Self::Flag(flag)),
            toml::Value::Integer(number) => Ok(Self::Number(number)),
            toml::Value::String(text) => {
                Placeholder::parse(&text).map_or_else(
                    || {
                        if text.contains(['{', '}']) {
                            Err(format!(
                                "`{text}` is not a known placeholder; placeholders cannot be embedded in text"
                            ))
                        } else {
                            Ok(Self::Text(text.clone()))
                        }
                    },
                    |placeholder| Ok(Self::Placeholder(placeholder)),
                )
            }
            other => Err(format!(
                "a tool argument must be a string, integer or boolean, not {}",
                other.type_str()
            )),
        }
    }
}

/// What a probe contributes to a tool call.
struct ProbeContext<'a> {
    repo: &'a Path,
    probe: &'a Probe,
}

impl ProbeContext<'_> {
    /// `None` when the probe has nothing for the placeholder (no position, no symbol).
    fn fill(&self, placeholder: Placeholder) -> Result<Option<Value>> {
        let file = self.probe.file();
        let position = self.probe.position();
        let zero_based = |one_based: u32, what: &str| {
            one_based
                .checked_sub(1)
                .ok_or_else(|| anyhow!("{what} is 1-based and cannot be 0"))
        };
        Ok(match placeholder {
            Placeholder::Repo => Some(path_value(self.repo)),
            Placeholder::File => Some(path_value(&file.in_repo(self.repo))),
            Placeholder::RelativeFile => Some(Value::from(file.as_str())),
            Placeholder::Line1 => position.map(|p| Value::from(p.line)),
            Placeholder::Line0 => position
                .map(|p| zero_based(p.line, "line").map(Value::from))
                .transpose()?,
            Placeholder::Character1 => position.map(|p| Value::from(p.character)),
            Placeholder::Character0 => position
                .map(|p| zero_based(p.character, "character").map(Value::from))
                .transpose()?,
            Placeholder::Symbol => self.probe.symbol().map(Value::from),
        })
    }

    fn resolve(&self, value: &ArgValue) -> Result<Option<Value>> {
        match value {
            ArgValue::Text(text) => Ok(Some(Value::from(text.as_str()))),
            ArgValue::Number(number) => Ok(Some(Value::from(*number))),
            ArgValue::Flag(flag) => Ok(Some(Value::from(*flag))),
            ArgValue::Placeholder(placeholder) => self.fill(*placeholder),
        }
    }
}

fn path_value(path: &Path) -> Value {
    Value::from(path.to_string_lossy().into_owned())
}

/// A tool of a comparison target and how to call it for one probe kind.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolBinding {
    /// MCP tool name.
    pub tool: String,
    /// Arguments of the call.
    #[serde(default)]
    pub arguments: BTreeMap<String, ArgValue>,
    /// Text whose occurrences in the answer are counted, for probes that expect a count.
    #[serde(default)]
    pub count_marker: Option<String>,
}

/// Which tool answers which probe kind; a missing binding makes that probe unsupported.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolMap {
    /// Answers `hover` probes.
    pub hover: Option<ToolBinding>,
    /// Answers `definition` probes.
    pub definition: Option<ToolBinding>,
    /// Answers `references` probes; needs `count_marker`.
    pub references: Option<ToolBinding>,
    /// Answers `document_symbols` probes.
    pub document_symbols: Option<ToolBinding>,
    /// Answers `diagnostics` probes; needs `count_marker`.
    pub diagnostics: Option<ToolBinding>,
}

impl ToolMap {
    const fn for_probe(&self, probe: &Probe) -> Option<&ToolBinding> {
        match probe {
            Probe::Hover { .. } => self.hover.as_ref(),
            Probe::Definition { .. } => self.definition.as_ref(),
            Probe::References { .. } => self.references.as_ref(),
            Probe::DocumentSymbols { .. } => self.document_symbols.as_ref(),
            Probe::Diagnostics { .. } => self.diagnostics.as_ref(),
        }
    }

    fn bindings(&self) -> impl Iterator<Item = &ToolBinding> {
        [
            &self.hover,
            &self.definition,
            &self.references,
            &self.document_symbols,
            &self.diagnostics,
        ]
        .into_iter()
        .flatten()
    }
}

/// One argument of the launch command.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum LaunchArg {
    /// Passed as written.
    Text(String),
    /// Replaced by the absolute repository path.
    Repo,
}

impl TryFrom<String> for LaunchArg {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value == Placeholder::Repo.token() {
            Ok(Self::Repo)
        } else if value.contains(['{', '}']) {
            Err(format!(
                "launch argument `{value}` may only use `{{repo}}` as a whole argument"
            ))
        } else {
            Ok(Self::Text(value))
        }
    }
}

/// A comparison MCP server, launched as written in its definition file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalTarget {
    /// Short name; appears in the report.
    pub name: String,
    /// Version or commit that must appear in `args`, so the launch cannot drift.
    pub pinned: String,
    /// The executable that starts the server; resolved from `PATH` and version-recorded.
    pub launcher: Executable,
    /// Launch arguments.
    pub args: Vec<LaunchArg>,
    /// Repository-relative paths the target writes; removed before and after every run.
    #[serde(default)]
    pub cleanup: Vec<RepoPath>,
    /// Bindings of probe kinds to tools.
    pub tools: ToolMap,
}

impl ExternalTarget {
    /// Loads and validates a target definition.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or parsed, the name is not a slug,
    /// the pin does not appear in the launch arguments, or a count-based binding has no
    /// `count_marker`.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read target {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid target {}", path.display()))
    }

    /// Parses and validates a target definition.
    ///
    /// # Errors
    ///
    /// See [`load`](Self::load).
    pub fn parse(text: &str) -> Result<Self> {
        let target: Self = toml::from_str(text).context("failed to parse the target")?;
        if !is_slug(&target.name) {
            bail!("target name `{}` must match [a-z0-9-]+", target.name);
        }
        let pinned_in_args = target
            .args
            .iter()
            .any(|arg| matches!(arg, LaunchArg::Text(text) if text.contains(&target.pinned)));
        if target.pinned.is_empty() || !pinned_in_args {
            bail!(
                "the pin `{}` does not appear in the launch arguments, so the launch is not pinned",
                target.pinned
            );
        }
        for path in &target.cleanup {
            let mut parts = Path::new(path.as_str()).components();
            let single = matches!(
                (parts.next(), parts.next()),
                (Some(std::path::Component::Normal(_)), None)
            );
            if !single {
                bail!(
                    "cleanup path `{}` must be a single directory or file name directly in the repository",
                    path.as_str()
                );
            }
        }
        for (kind, binding) in [
            ("references", &target.tools.references),
            ("diagnostics", &target.tools.diagnostics),
        ] {
            if binding
                .as_ref()
                .is_some_and(|binding| binding.count_marker.is_none())
            {
                bail!("tools.{kind} counts results and needs a `count_marker`");
            }
        }
        Ok(target)
    }

    /// Names of the tools the target must list, checked right after startup.
    pub fn required_tools(&self) -> impl Iterator<Item = &str> {
        self.tools.bindings().map(|binding| binding.tool.as_str())
    }

    /// Absolute paths below `repo` that this target writes and that must not outlive a run.
    #[must_use]
    pub fn cleanup_paths(&self, repo: &Path) -> Vec<PathBuf> {
        self.cleanup.iter().map(|path| path.in_repo(repo)).collect()
    }

    /// Launch arguments with the repository filled in.
    #[must_use]
    pub fn launch_args(&self, repo: &Path) -> Vec<OsString> {
        self.args
            .iter()
            .map(|arg| match arg {
                LaunchArg::Text(text) => OsString::from(text),
                LaunchArg::Repo => repo.as_os_str().to_owned(),
            })
            .collect()
    }

    /// The call for `probe`, or `None` when the target has no tool for it or the
    /// probe lacks something a placeholder needs.
    fn request(&self, probe: &Probe, repo: &Path) -> Result<Option<CallToolRequestParams>> {
        let Some(binding) = self.tools.for_probe(probe) else {
            return Ok(None);
        };
        let context = ProbeContext { repo, probe };
        let mut arguments = JsonObject::new();
        for (name, value) in &binding.arguments {
            match context.resolve(value)? {
                Some(value) => {
                    arguments.insert(name.clone(), value);
                }
                None => return Ok(None),
            }
        }
        Ok(Some(
            CallToolRequestParams::new(binding.tool.clone()).with_arguments(arguments),
        ))
    }

    fn verdict(&self, probe: &Probe, result: &CallToolResult) -> Result<(), Incorrect> {
        let binding = self
            .tools
            .for_probe(probe)
            .ok_or_else(|| Incorrect("the target has no tool for this probe".to_owned()))?;
        textual_verdict(probe, binding.count_marker.as_deref(), &result_text(result))
    }
}

/// All text a result carries: its text blocks, then its structured content.
fn result_text(result: &CallToolResult) -> String {
    let mut text = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(structured) = &result.structured_content {
        text.push('\n');
        text.push_str(&structured.to_string());
    }
    text
}

fn textual_verdict(probe: &Probe, marker: Option<&str>, text: &str) -> Result<(), Incorrect> {
    use crate::scenario::DiagnosticsExpect;

    let missing = |what: &str| Err(Incorrect(format!("the answer does not mention {what}")));
    let counted = || marker.map_or(0, |marker| text.matches(marker).count());
    match probe {
        Probe::Hover { contains, .. } if !text.contains(contains.as_str()) => {
            missing(&format!("`{contains}`"))
        }
        Probe::Definition { uri_suffix, .. } if !text.contains(uri_suffix.as_str()) => {
            missing(&format!("`{uri_suffix}`"))
        }
        Probe::DocumentSymbols { symbol, .. } if !text.contains(symbol.as_str()) => {
            missing(&format!("the symbol `{symbol}`"))
        }
        Probe::References { min_count, .. } if counted() < *min_count => Err(Incorrect(format!(
            "expected >= {min_count} references, counted {}",
            counted()
        ))),
        Probe::Diagnostics {
            expect: DiagnosticsExpect::AtLeast { count },
            ..
        } if counted() < *count => Err(Incorrect(format!(
            "expected >= {count} diagnostics, counted {}",
            counted()
        ))),
        Probe::Diagnostics {
            expect: DiagnosticsExpect::NoErrors,
            ..
        } if counted() > 0 => Err(Incorrect(format!("{} error diagnostics", counted()))),
        _ => Ok(()),
    }
}

/// The system under measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// mcpls at the given binary, driving the scenario's language server.
    Mcpls {
        /// The mcpls executable.
        binary: PathBuf,
    },
    /// A comparison server.
    External(Box<ExternalTarget>),
}

/// The tool call for a probe, or the fact that the target cannot make one.
#[derive(Debug)]
pub enum ProbeCall {
    /// Send this request.
    Call(CallToolRequestParams),
    /// The target has no tool for the probe; record it as unsupported.
    Unsupported,
}

impl Target {
    /// How this target's answers are checked.
    #[must_use]
    pub const fn verification(&self) -> Verification {
        match self {
            Self::Mcpls { .. } => Verification::Structured,
            Self::External(_) => Verification::Textual,
        }
    }

    /// Builds the call for `probe` against the repository at `repo`.
    ///
    /// # Errors
    ///
    /// Returns an error when the arguments cannot be built.
    pub fn request(&self, probe: &Probe, repo: &Path) -> Result<ProbeCall> {
        match self {
            Self::Mcpls { .. } => probe.request(repo).map(ProbeCall::Call),
            Self::External(target) => Ok(target
                .request(probe, repo)?
                .map_or(ProbeCall::Unsupported, ProbeCall::Call)),
        }
    }

    /// Checks a successful result against the probe's expectation.
    ///
    /// # Errors
    ///
    /// Returns [`Incorrect`] when the answer does not meet it.
    pub fn verdict(&self, probe: &Probe, result: &CallToolResult) -> Result<(), Incorrect> {
        match self {
            Self::Mcpls { .. } => probe.verdict(result),
            Self::External(target) => target.verdict(probe, result),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::assert_matches;

    use mcpls_core::bridge::Position2D;
    use rmcp::model::ContentBlock;

    use super::*;
    use crate::scenario::DiagnosticsExpect;

    const SERENA: &str = include_str!("../targets/serena.toml");
    const LSMCP: &str = include_str!("../targets/lsmcp.toml");

    fn file() -> RepoPath {
        RepoPath::try_from("src/useForm.ts".to_owned()).unwrap()
    }

    fn definition(symbol: Option<&str>) -> Probe {
        Probe::Definition {
            file: file(),
            position: Position2D {
                line: 5,
                character: 10,
            },
            uri_suffix: "src/utils/deepEqual.ts".to_owned(),
            symbol: symbol.map(str::to_owned),
        }
    }

    fn text_result(text: &str) -> CallToolResult {
        CallToolResult::success(vec![ContentBlock::text(text)])
    }

    #[test]
    fn bundled_targets_parse() {
        for text in [SERENA, LSMCP] {
            let target = ExternalTarget::parse(text).unwrap();
            assert!(target.required_tools().count() > 0);
        }
    }

    #[test]
    fn serena_is_pinned_and_cleans_its_state_directory() {
        let target = ExternalTarget::parse(SERENA).unwrap();
        assert_eq!(target.pinned.len(), 40);
        assert_eq!(
            target.cleanup_paths(Path::new("/r")),
            [PathBuf::from("/r/.serena")]
        );
        let args = target.launch_args(Path::new("/r"));
        assert!(args.iter().any(|a| a == "/r"));
        assert!(args.iter().any(|a| a == "--enable-web-dashboard"));
    }

    #[test]
    fn placeholders_fill_from_the_probe() {
        let target = ExternalTarget::parse(LSMCP).unwrap();
        let ProbeCall::Call(call) = Target::External(Box::new(target))
            .request(&definition(Some("deepEqual")), Path::new("/r"))
            .unwrap()
        else {
            panic!("lsmcp supports definitions");
        };
        let args = call.arguments.unwrap();
        assert_eq!(args["root"], "/r");
        assert_eq!(args["relativePath"], "src/useForm.ts");
        assert_eq!(args["line"], 5);
        assert_eq!(args["column"], 9);
        assert_eq!(args["symbolName"], "deepEqual");
    }

    #[test]
    fn a_probe_without_what_a_placeholder_needs_is_unsupported() {
        let target = Target::External(Box::new(ExternalTarget::parse(LSMCP).unwrap()));
        assert_matches!(
            target.request(&definition(None), Path::new("/r")).unwrap(),
            ProbeCall::Unsupported
        );
    }

    #[test]
    fn a_probe_kind_without_a_binding_is_unsupported() {
        let target = Target::External(Box::new(ExternalTarget::parse(SERENA).unwrap()));
        let hover = Probe::Hover {
            file: file(),
            position: Position2D {
                line: 1,
                character: 1,
            },
            contains: "x".to_owned(),
            symbol: Some("x".to_owned()),
        };
        assert_matches!(
            target.request(&hover, Path::new("/r")).unwrap(),
            ProbeCall::Unsupported
        );
    }

    #[test]
    fn unknown_or_embedded_placeholders_are_refused_at_load() {
        let bad = SERENA.replacen("{repo}", "{reppo}", 1);
        assert!(ExternalTarget::parse(&bad).is_err());
        let embedded = SERENA.replacen("\"{repo}\"", "\"--x={repo}\"", 1);
        assert!(ExternalTarget::parse(&embedded).is_err());
    }

    #[test]
    fn cleanup_paths_must_be_a_single_component() {
        for bad in ["a/b", "./.serena", "a/../b", "sub/.serena"] {
            let text = SERENA.replacen(
                "cleanup = [\".serena\"]",
                &format!("cleanup = [\"{bad}\"]"),
                1,
            );
            assert_ne!(text, SERENA);
            assert!(ExternalTarget::parse(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_unpinned_launch_is_refused() {
        let unpinned = SERENA.replacen("pinned = \"", "pinned = \"0000", 1);
        assert!(ExternalTarget::parse(&unpinned).is_err());
    }

    #[test]
    fn counting_bindings_need_a_marker() {
        let text: Vec<&str> = LSMCP
            .lines()
            .filter(|line| !line.starts_with("count_marker"))
            .collect();
        assert!(text.len() < LSMCP.lines().count());
        let text = text.join("\n");
        let error = ExternalTarget::parse(&text).unwrap_err();
        assert!(error.to_string().contains("count_marker"), "{error}");
    }

    #[test]
    fn textual_verdicts_check_substrings_and_counts() {
        let probe = definition(Some("deepEqual"));
        assert!(textual_verdict(&probe, None, "at src/utils/deepEqual.ts:3").is_ok());
        assert!(textual_verdict(&probe, None, "elsewhere").is_err());

        let references = Probe::References {
            file: file(),
            position: Position2D {
                line: 1,
                character: 1,
            },
            min_count: 2,
            symbol: None,
        };
        assert!(textual_verdict(&references, Some("ref:"), "ref: a\nref: b").is_ok());
        assert!(textual_verdict(&references, Some("ref:"), "ref: a").is_err());

        let clean = Probe::Diagnostics {
            file: file(),
            expect: DiagnosticsExpect::NoErrors,
        };
        assert!(textual_verdict(&clean, Some("error"), "all good").is_ok());
        assert!(textual_verdict(&clean, Some("error"), "1 error").is_err());
    }

    #[test]
    fn result_text_joins_blocks_and_structured_content() {
        let mut result = text_result("one");
        result.structured_content = Some(serde_json::json!({"two": 2}));
        let text = result_text(&result);
        assert!(text.contains("one") && text.contains("\"two\""));
    }

    #[test]
    fn mcpls_target_uses_structured_verification() {
        let target = Target::Mcpls {
            binary: PathBuf::from("/bin/mcpls"),
        };
        assert_eq!(target.verification(), Verification::Structured);
        let external = Target::External(Box::new(ExternalTarget::parse(SERENA).unwrap()));
        assert_eq!(external.verification(), Verification::Textual);
    }
}
