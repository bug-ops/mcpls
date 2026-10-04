//! Translation of scenario probes into MCP tool calls and verdicts on their answers.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use mcpls_core::bridge::{
    DefinitionResult, DiagnosticSeverity, DiagnosticsResult, DocumentSymbolsResult, HoverResult,
    Position2D, ReferencesResult, Symbol,
};
use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::report::Region;
use crate::scenario::{DiagnosticsExpect, Probe, RepoPath};

/// A probe's answer did not meet its expectation; carries the explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incorrect(pub String);

#[derive(Serialize)]
struct PositionArgs {
    file_path: String,
    line: u32,
    character: u32,
}

#[derive(Serialize)]
struct ReferencesArgs {
    #[serde(flatten)]
    position: PositionArgs,
    include_declaration: bool,
}

#[derive(Serialize)]
struct FileArgs {
    file_path: String,
}

fn object(args: &impl Serialize) -> Result<JsonObject> {
    match serde_json::to_value(args).context("failed to serialize tool arguments")? {
        serde_json::Value::Object(map) => Ok(map),
        other => Err(anyhow!("tool arguments are not an object: {other}")),
    }
}

fn file_path(repo: &Path, file: &RepoPath) -> String {
    file.in_repo(repo).to_string_lossy().into_owned()
}

fn position_args(repo: &Path, file: &RepoPath, at: &Position2D) -> PositionArgs {
    PositionArgs {
        file_path: file_path(repo, file),
        line: at.line,
        character: at.character,
    }
}

impl Probe {
    /// The region this probe's samples are recorded under.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_bench::report::Region;
    /// use mcpls_bench::scenario::{Probe, RepoPath};
    ///
    /// let probe = Probe::DocumentSymbols {
    ///     file: RepoPath::try_from("src/lib.rs".to_owned()).unwrap(),
    ///     symbol: "User".to_owned(),
    /// };
    /// assert_eq!(probe.region(), Region::DocumentSymbols);
    /// ```
    #[must_use]
    pub const fn region(&self) -> Region {
        match self {
            Self::Hover { .. } => Region::Hover,
            Self::Definition { .. } => Region::Definition,
            Self::References { .. } => Region::References,
            Self::DocumentSymbols { .. } => Region::DocumentSymbols,
            Self::Diagnostics { .. } => Region::Diagnostics,
        }
    }

    /// The file this probe queries.
    #[must_use]
    pub const fn file(&self) -> &RepoPath {
        match self {
            Self::Hover { file, .. }
            | Self::Definition { file, .. }
            | Self::References { file, .. }
            | Self::DocumentSymbols { file, .. }
            | Self::Diagnostics { file, .. } => file,
        }
    }

    /// The 1-based position this probe queries, for the kinds that have one.
    #[must_use]
    pub const fn position(&self) -> Option<&Position2D> {
        match self {
            Self::Hover { position, .. }
            | Self::Definition { position, .. }
            | Self::References { position, .. } => Some(position),
            Self::DocumentSymbols { .. } | Self::Diagnostics { .. } => None,
        }
    }

    /// The identifier this probe is about, for targets that address symbols by name.
    #[must_use]
    pub fn symbol(&self) -> Option<&str> {
        match self {
            Self::Hover { symbol, .. }
            | Self::Definition { symbol, .. }
            | Self::References { symbol, .. } => symbol.as_deref(),
            Self::DocumentSymbols { symbol, .. } => Some(symbol),
            Self::Diagnostics { .. } => None,
        }
    }

    /// Builds the MCP `tools/call` request for this probe against the repository at `repo`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::Path;
    /// use mcpls_bench::scenario::{Probe, RepoPath};
    ///
    /// let probe = Probe::DocumentSymbols {
    ///     file: RepoPath::try_from("src/lib.rs".to_owned()).unwrap(),
    ///     symbol: "User".to_owned(),
    /// };
    /// let request = probe.request(Path::new("/repo")).unwrap();
    /// assert_eq!(request.name, "get_document_symbols");
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the arguments cannot be serialized.
    pub fn request(&self, repo: &Path) -> Result<CallToolRequestParams> {
        let (tool, arguments) = match self {
            Self::Hover { file, position, .. } => {
                ("get_hover", object(&position_args(repo, file, position))?)
            }
            Self::Definition { file, position, .. } => (
                "get_definition",
                object(&position_args(repo, file, position))?,
            ),
            Self::References { file, position, .. } => (
                "get_references",
                object(&ReferencesArgs {
                    position: position_args(repo, file, position),
                    include_declaration: true,
                })?,
            ),
            Self::DocumentSymbols { file, .. } => (
                "get_document_symbols",
                object(&FileArgs {
                    file_path: file_path(repo, file),
                })?,
            ),
            Self::Diagnostics { file, .. } => (
                "get_diagnostics",
                object(&FileArgs {
                    file_path: file_path(repo, file),
                })?,
            ),
        };
        Ok(CallToolRequestParams::new(tool).with_arguments(arguments))
    }

    /// Checks a successful tool result against this probe's expectation.
    ///
    /// # Errors
    ///
    /// Returns [`Incorrect`] when the payload cannot be decoded or does not meet the expectation.
    pub fn verdict(&self, result: &CallToolResult) -> Result<(), Incorrect> {
        match self {
            Self::Hover { contains, .. } => {
                let hover: HoverResult = payload(result)?;
                expect(hover.contents.contains(contains.as_str()), || {
                    format!("hover does not contain `{contains}`: {}", hover.contents)
                })
            }
            Self::Definition { uri_suffix, .. } => {
                let definition: DefinitionResult = payload(result)?;
                expect(
                    definition
                        .locations
                        .iter()
                        .any(|l| l.uri.ends_with(uri_suffix.as_str())),
                    || format!("no definition uri ends with `{uri_suffix}`"),
                )
            }
            Self::References { min_count, .. } => {
                let references: ReferencesResult = payload(result)?;
                expect(references.locations.len() >= *min_count, || {
                    format!(
                        "expected >= {min_count} references, got {}",
                        references.locations.len()
                    )
                })
            }
            Self::DocumentSymbols { symbol, .. } => {
                let symbols: DocumentSymbolsResult = payload(result)?;
                expect(contains_symbol(&symbols.symbols, symbol), || {
                    format!("no symbol named `{symbol}`")
                })
            }
            Self::Diagnostics { expect: wanted, .. } => {
                let diagnostics: DiagnosticsResult = payload(result)?;
                match wanted {
                    DiagnosticsExpect::NoErrors => {
                        let errors = diagnostics
                            .diagnostics
                            .iter()
                            .filter(|d| d.severity == DiagnosticSeverity::Error)
                            .count();
                        expect(errors == 0, || format!("{errors} error diagnostics"))
                    }
                    DiagnosticsExpect::AtLeast { count } => {
                        expect(diagnostics.diagnostics.len() >= *count, || {
                            format!(
                                "expected >= {count} diagnostics, got {}",
                                diagnostics.diagnostics.len()
                            )
                        })
                    }
                }
            }
        }
    }
}

fn expect(condition: bool, detail: impl FnOnce() -> String) -> Result<(), Incorrect> {
    if condition {
        Ok(())
    } else {
        Err(Incorrect(detail()))
    }
}

fn contains_symbol(symbols: &[Symbol], name: &str) -> bool {
    symbols.iter().any(|s| {
        s.name == name
            || s.children
                .as_deref()
                .is_some_and(|children| contains_symbol(children, name))
    })
}

/// Decodes a tool result from `structuredContent`, falling back to the first text block.
fn payload<T: DeserializeOwned>(result: &CallToolResult) -> Result<T, Incorrect> {
    if let Some(value) = &result.structured_content {
        return serde_json::from_value(value.clone())
            .map_err(|e| Incorrect(format!("undecodable result: {e}")));
    }
    let text = result
        .content
        .iter()
        .find_map(|block| block.as_text())
        .ok_or_else(|| Incorrect("result carries no text content".to_owned()))?;
    serde_json::from_str(&text.text).map_err(|e| Incorrect(format!("undecodable result: {e}")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use rmcp::model::ContentBlock;
    use serde_json::json;

    use super::*;
    use crate::scenario::RepoPath;

    fn hover_probe(contains: &str) -> Probe {
        Probe::Hover {
            file: RepoPath::try_from("src/lib.rs".to_owned()).unwrap(),
            position: Position2D {
                line: 1,
                character: 1,
            },
            contains: contains.to_owned(),
            symbol: None,
        }
    }

    #[test]
    fn hover_verdict_checks_substring_in_text_content() {
        let result = CallToolResult::success(vec![ContentBlock::text(
            json!({"contents": "struct User", "range": null}).to_string(),
        )]);
        assert_eq!(hover_probe("User").verdict(&result), Ok(()));
        assert!(hover_probe("Missing").verdict(&result).is_err());
    }

    #[test]
    fn structured_content_is_preferred() {
        let probe = Probe::References {
            file: RepoPath::try_from("src/lib.rs".to_owned()).unwrap(),
            position: Position2D {
                line: 1,
                character: 1,
            },
            min_count: 1,
            symbol: None,
        };
        let result = CallToolResult::structured(json!({"locations": []}));
        assert!(probe.verdict(&result).is_err());
    }

    #[test]
    fn request_uses_absolute_file_path_and_tool_name() {
        let request = hover_probe("x").request(Path::new("/repo")).unwrap();
        assert_eq!(request.name, "get_hover");
        let args = request.arguments.unwrap();
        let expected = Path::new("/repo").join("src/lib.rs");
        assert_eq!(args["file_path"], expected.to_string_lossy().as_ref());
        assert_eq!(args["line"], 1);
    }
}
