//! Payloads describing how a symbol name resolved, or failed to.

use std::fmt;

use serde::Serialize;

/// Maximum candidates named in the `Display` text of an ambiguity error;
/// the structured payload carries all of them.
const MAX_DISPLAYED_CANDIDATES: usize = 10;

/// One symbol a name resolved to, with enough context to tell it apart from
/// its namesakes and to address it by position instead.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::SymbolCandidate;
///
/// let candidate = SymbolCandidate {
///     name: "new".to_string(),
///     kind: 6,
///     kind_name: "Method".to_string(),
///     container: Some("Config".to_string()),
///     line: 12,
///     character: 8,
/// };
/// assert_eq!(candidate.to_string(), "Method `new` in `Config` at 12:8");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolCandidate {
    /// The symbol's name as the server reports it.
    pub name: String,
    /// Numeric LSP `SymbolKind`.
    pub kind: u32,
    /// Readable name of [`Self::kind`].
    pub kind_name: String,
    /// Enclosing symbol, when the server reports one.
    pub container: Option<String>,
    /// 1-based line of the identifier.
    pub line: u32,
    /// 1-based character of the identifier.
    pub character: u32,
}

impl fmt::Display for SymbolCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} `{}`", self.kind_name, self.name)?;
        if let Some(container) = &self.container {
            write!(f, " in `{container}`")?;
        }
        write!(f, " at {}:{}", self.line, self.character)
    }
}

/// Why a symbol name could not be resolved to exactly one position.
///
/// Carried as the structured `data` of the `INVALID_PARAMS` error, tagged by
/// `resolution`.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::SymbolResolutionData;
///
/// let missing = SymbolResolutionData::NotDefinedInFile { name: "Config".to_string() };
/// assert_eq!(
///     serde_json::to_value(&missing).unwrap(),
///     serde_json::json!({"resolution": "not_defined_in_file", "name": "Config"})
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "resolution", rename_all = "snake_case")]
pub enum SymbolResolutionData {
    /// The name matches several symbols; nothing was picked.
    Ambiguous {
        /// The requested name.
        name: String,
        /// The matching symbols, capped.
        candidates: Vec<SymbolCandidate>,
        /// More symbols match than `candidates` lists.
        truncated: bool,
    },
    /// No symbol of that name (and kind/container) is defined in the file.
    NotFound {
        /// The requested name.
        name: String,
        /// Symbols of that name excluded by the kind or container filter.
        excluded_by_filters: usize,
    },
    /// The name occurs in the file but no symbol defines it there (an import
    /// or a plain reference).
    NotDefinedInFile {
        /// The requested name.
        name: String,
    },
    /// A symbol matched but its identifier position could not be verified
    /// against the document text, so no query was made.
    PositionUnverified {
        /// The requested name.
        name: String,
        /// The symbol that matched.
        candidate: SymbolCandidate,
    },
}

impl fmt::Display for SymbolResolutionData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ambiguous {
                name,
                candidates,
                truncated,
            } => {
                write!(
                    f,
                    "symbol `{name}` is ambiguous ({} candidates{}): ",
                    candidates.len(),
                    if *truncated { ", more not listed" } else { "" }
                )?;
                for (index, candidate) in
                    candidates.iter().take(MAX_DISPLAYED_CANDIDATES).enumerate()
                {
                    if index > 0 {
                        f.write_str("; ")?;
                    }
                    candidate.fmt(f)?;
                }
                if candidates.len() > MAX_DISPLAYED_CANDIDATES {
                    f.write_str("; ...")?;
                }
                f.write_str(
                    ". Narrow it with `symbol_kind` or `container`, or address it by line and character",
                )
            }
            Self::NotFound {
                name,
                excluded_by_filters,
            } => {
                write!(f, "no symbol named `{name}` is defined in this file")?;
                if *excluded_by_filters > 0 {
                    write!(
                        f,
                        " ({excluded_by_filters} with that name excluded by `symbol_kind`/`container`)"
                    )?;
                }
                f.write_str("; try `workspace_symbol_search` or address it by line and character")
            }
            Self::NotDefinedInFile { name } => write!(
                f,
                "`{name}` is not defined in this file (it only occurs as a reference or import); \
                 find its definition with `workspace_symbol_search` or address it by line and character"
            ),
            Self::PositionUnverified { name, candidate } => write!(
                f,
                "could not verify where the identifier `{name}` is ({candidate}), so nothing was \
                 queried; address it by line and character"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ambiguity_display_lists_at_most_ten_candidates_and_the_hint() {
        let candidates: Vec<SymbolCandidate> = (1..=12)
            .map(|line| SymbolCandidate {
                name: "new".to_string(),
                kind: 6,
                kind_name: "Method".to_string(),
                container: Some("Foo".to_string()),
                line,
                character: 1,
            })
            .collect();
        let text = SymbolResolutionData::Ambiguous {
            name: "new".to_string(),
            candidates,
            truncated: false,
        }
        .to_string();

        assert!(text.contains("12 candidates"), "{text}");
        assert!(
            text.contains("at 10:1") && !text.contains("at 11:1"),
            "{text}"
        );
        assert!(text.contains("; ..."), "{text}");
        assert!(text.contains("`symbol_kind` or `container`"), "{text}");
    }
}
