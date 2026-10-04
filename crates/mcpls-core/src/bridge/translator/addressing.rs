//! Symbol-name addressing for position-based tools.
//!
//! A caller may name a symbol instead of counting a line and column. The name
//! is resolved against the file's `textDocument/documentSymbol` answer, the
//! identifier position of the one matching symbol is verified against the
//! tracked document text, and the tool then runs at that position exactly as
//! if the caller had supplied it. A name that matches several symbols, none,
//! or whose identifier cannot be located unambiguously yields a typed
//! [`SymbolResolutionData`] error instead of a guess.

use lsp_types::{DocumentSymbolResponse, SymbolKind};
use schemars::JsonSchema;
use serde::Serialize;

use super::Translator;
use super::dto::{Position, Position2D};
use super::encoding_ctx::EncodingCtx;
use super::routing::{Capability, IndexingGate};
use crate::bridge::ClientPath;
use crate::bridge::encoding::{EncodingConverter, PositionEncoding};
use crate::bridge::state::uri_to_path;
use crate::error::{Error, Result, SymbolCandidate, SymbolResolutionData};

/// Longest accepted symbol name, container or kind text, in bytes.
pub const MAX_SYMBOL_NAME_BYTES: usize = 256;

/// Candidates listed in an ambiguity error; more are reported as truncated.
const MAX_CANDIDATES: usize = 50;

/// Symbols examined in one document-symbol answer.
const MAX_FLATTENED_SYMBOLS: usize = 20_000;

/// Lines of a symbol's range searched for its identifier.
const MAX_WINDOW_LINES: u32 = 500;

/// Lines of the document scanned to tell "referenced" from "absent".
const MAX_SCAN_LINES: usize = 50_000;

/// Why [`SymbolName::try_new`] rejected a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SymbolNameError {
    /// The name is empty or only whitespace.
    #[error("symbol names must not be blank")]
    Blank,
    /// The name is longer than [`MAX_SYMBOL_NAME_BYTES`].
    #[error("symbol names must be at most {MAX_SYMBOL_NAME_BYTES} bytes")]
    TooLong,
}

/// A validated symbol or container name: surrounding whitespace trimmed,
/// non-blank and bounded.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::SymbolName;
///
/// assert_eq!(SymbolName::try_new("parse_config").unwrap().as_str(), "parse_config");
/// assert_eq!(SymbolName::try_new(" parse_config ").unwrap().as_str(), "parse_config");
/// assert!(SymbolName::try_new("  ").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolName(String);

impl SymbolName {
    /// Validate `raw` after trimming surrounding whitespace, which is not
    /// part of the name.
    ///
    /// # Errors
    ///
    /// [`SymbolNameError::Blank`] or [`SymbolNameError::TooLong`] (judged on
    /// the trimmed text).
    pub fn try_new(raw: impl Into<String>) -> std::result::Result<Self, SymbolNameError> {
        let raw = raw.into().trim().to_string();
        if raw.is_empty() {
            return Err(SymbolNameError::Blank);
        }
        if raw.len() > MAX_SYMBOL_NAME_BYTES {
            return Err(SymbolNameError::TooLong);
        }
        Ok(Self(raw))
    }

    /// The name text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A symbol named by the caller, optionally narrowed by kind and container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolQuery {
    /// The symbol's name, optionally qualified (`Type::method`, `Type.method`).
    pub name: SymbolName,
    /// Keep only symbols of this kind.
    pub kind: Option<SymbolKind>,
    /// Keep only symbols directly inside a container of this name.
    pub container: Option<SymbolName>,
}

/// How a tool addresses its target: by 1-based position or by symbol name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolTarget {
    /// An exact 1-based position.
    Position(Position),
    /// A symbol resolved by name within the file.
    Name(SymbolQuery),
}

/// The tools that accept a [`SymbolTarget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressableTool {
    /// `get_hover`.
    Hover,
    /// `get_definition`.
    Definition,
    /// `get_references`.
    References,
    /// `go_to_implementation`.
    Implementation,
    /// `go_to_type_definition`.
    TypeDefinition,
    /// `prepare_call_hierarchy`.
    PrepareCallHierarchy,
    /// `rename_symbol`.
    Rename,
}

impl AddressableTool {
    const fn capability(self) -> Capability {
        match self {
            Self::Hover => Capability::Hover,
            Self::Definition => Capability::Definition,
            Self::References => Capability::References,
            Self::Implementation => Capability::Implementation,
            Self::TypeDefinition => Capability::TypeDefinition,
            Self::PrepareCallHierarchy => Capability::CallHierarchy,
            Self::Rename => Capability::Rename,
        }
    }
}

/// Where the queried position of a name-addressed call came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PositionSource {
    /// The server's `selectionRange` start, verified to spell the name.
    SelectionRange,
    /// The only whole-word occurrence of the name inside the symbol's range,
    /// found in the document text because the server gave no usable
    /// `selectionRange`.
    Inferred,
}

/// The symbol a name resolved to, reported so the caller can audit and reuse
/// the queried position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ResolvedSymbol {
    /// The symbol's name as the server reports it.
    pub name: String,
    /// Numeric LSP `SymbolKind`.
    pub kind: u32,
    /// Enclosing symbol, when the server reports one.
    pub container: Option<String>,
    /// The 1-based position that was queried.
    pub position: Position2D,
    /// How `position` was determined.
    pub position_source: PositionSource,
}

/// A tool result together with the symbol its name resolved to, when the call
/// was name-addressed.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Addressed<T> {
    /// The tool's own result.
    #[serde(flatten)]
    pub result: T,
    /// Set only for a name-addressed call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_symbol: Option<ResolvedSymbol>,
}

/// The position a tool is to run at, and how it was obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    /// The 1-based position to query.
    pub position: Position,
    /// The resolved symbol; `None` when the caller gave the position.
    pub resolved: Option<ResolvedSymbol>,
}

/// One symbol of a document-symbol answer, flattened.
#[derive(Debug, Clone)]
struct SymbolEntry {
    name: String,
    kind: SymbolKind,
    container: Option<String>,
    range: lsp_types::Range,
    selection_range: Option<lsp_types::Range>,
}

fn flatten_symbols(response: DocumentSymbolResponse) -> Vec<SymbolEntry> {
    let mut entries = Vec::new();
    match response {
        DocumentSymbolResponse::SymbolInformationList(symbols) => {
            for symbol in symbols.into_iter().take(MAX_FLATTENED_SYMBOLS) {
                let base = symbol.base_symbol_information;
                entries.push(SymbolEntry {
                    name: base.name,
                    kind: base.kind,
                    container: base.container_name,
                    range: symbol.location.range,
                    selection_range: None,
                });
            }
        }
        DocumentSymbolResponse::DocumentSymbolList(symbols) => {
            let mut stack: Vec<(lsp_types::DocumentSymbol, Option<String>)> =
                symbols.into_iter().rev().map(|s| (s, None)).collect();
            while let Some((symbol, parent)) = stack.pop() {
                if entries.len() >= MAX_FLATTENED_SYMBOLS {
                    break;
                }
                let children = symbol.children.unwrap_or_default();
                for child in children.into_iter().rev() {
                    stack.push((child, Some(symbol.name.clone())));
                }
                entries.push(SymbolEntry {
                    name: symbol.name,
                    kind: symbol.kind,
                    container: parent,
                    range: symbol.range,
                    selection_range: Some(symbol.selection_range),
                });
            }
        }
    }
    entries
}

/// The simple identifier of `raw` and the qualifier written before it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NameParts {
    simple: String,
    qualifier: Option<String>,
}

/// Whether `raw` contains the C++ `operator` keyword as a word, so
/// `operator<` and `Foo::operator()` qualify but `operatorCount(int)` and
/// `get_operator(x)` do not.
fn names_an_operator(raw: &str) -> bool {
    raw.match_indices("operator").any(|(start, keyword)| {
        let before = raw.get(..start).and_then(|head| head.chars().next_back());
        let after = raw
            .get(start.saturating_add(keyword.len())..)
            .and_then(|tail| tail.chars().next());
        !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
    })
}

/// `raw` without generic arguments, or `raw` itself when its angle brackets
/// do not pair up or it names an operator (`operator<`, `operator<=>`).
fn strip_generics(raw: &str) -> String {
    if names_an_operator(raw) {
        return raw.to_string();
    }
    let mut depth = 0_u32;
    let mut stripped = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '<' => depth = depth.saturating_add(1),
            '>' => {
                if depth == 0 {
                    return raw.to_string();
                }
                depth = depth.saturating_sub(1);
            }
            _ if depth == 0 => stripped.push(ch),
            _ => {}
        }
    }
    if depth == 0 {
        stripped
    } else {
        raw.to_string()
    }
}

/// `receiver` of a Go-style `(*T).Method` name, without `*`/`&`.
fn receiver_and_rest(raw: &str) -> Option<(String, &str)> {
    let inner = raw.strip_prefix('(')?;
    let close = inner.find(')')?;
    let rest = inner.get(close.saturating_add(1)..)?.strip_prefix('.')?;
    let receiver = strip_generics(inner.get(..close)?)
        .trim_start_matches(['*', '&'])
        .trim()
        .to_string();
    Some((receiver, rest))
}

/// `raw` without a trailing balanced parameter list (`bar(int)` is `bar`), or
/// `raw` itself when there is none or it names an operator (`operator()`).
fn strip_params(raw: &str) -> &str {
    if names_an_operator(raw) || !raw.ends_with(')') {
        return raw;
    }
    let mut depth = 0_u32;
    for (index, ch) in raw.char_indices().rev() {
        match ch {
            ')' => depth = depth.saturating_add(1),
            '(' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return raw
                        .get(..index)
                        .filter(|head| !head.is_empty())
                        .unwrap_or(raw);
                }
            }
            _ => {}
        }
    }
    raw
}

fn name_parts(raw: &str) -> NameParts {
    if let Some((receiver, rest)) = receiver_and_rest(raw) {
        return NameParts {
            simple: strip_generics(strip_params(rest)),
            qualifier: Some(receiver),
        };
    }
    let stripped = strip_generics(strip_params(raw));
    let mut segments = stripped.rsplit("::").flat_map(|part| part.rsplit('.'));
    let simple = segments.next().unwrap_or_default().to_string();
    let qualifier = segments.next().map(str::to_string);
    NameParts { simple, qualifier }
}

/// The simple name a container is known by: `impl<T> Tr for Foo<T>` is `Foo`.
fn container_name(raw: &str) -> String {
    let trimmed = raw.trim();
    let target = match trimmed.strip_prefix("impl") {
        Some(rest) if rest.starts_with(|c: char| c.is_whitespace() || c == '<') => {
            let rest = strip_generics_prefix(rest);
            rest.rsplit_once(" for ")
                .map_or_else(|| rest.trim(), |(_, ty)| ty.trim())
        }
        _ => trimmed,
    };
    name_parts(target).simple
}

/// `rest` without a leading `<...>` parameter list.
fn strip_generics_prefix(rest: &str) -> &str {
    let Some(inner) = rest.strip_prefix('<') else {
        return rest.trim_start();
    };
    let mut depth = 1_u32;
    for (index, ch) in inner.char_indices() {
        match ch {
            '<' => depth = depth.saturating_add(1),
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return inner
                        .get(index.saturating_add(1)..)
                        .unwrap_or_default()
                        .trim_start();
                }
            }
            _ => {}
        }
    }
    rest
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// Byte offsets of the whole-word occurrences of `word` in `text`.
fn whole_word_occurrences(text: &str, word: &str) -> Vec<usize> {
    if word.is_empty() {
        return Vec::new();
    }
    text.match_indices(word)
        .filter(|(start, _)| {
            let before = text.get(..*start).and_then(|head| head.chars().next_back());
            let after = text
                .get(start.saturating_add(word.len())..)
                .and_then(|tail| tail.chars().next());
            !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
        })
        .map(|(start, _)| start)
        .collect()
}

/// The result of matching a query against a document's symbols.
struct Selection<'a> {
    matched: Vec<&'a SymbolEntry>,
    excluded_by_filters: usize,
}

/// Names a symbol is nested under: its parent/`containerName` plus any
/// qualifier written in its own name.
fn effective_containers(entry: &SymbolEntry) -> Vec<String> {
    let mut containers: Vec<String> = entry.container.iter().map(|c| container_name(c)).collect();
    containers.extend(name_parts(&entry.name).qualifier);
    containers
}

fn select_candidates<'a>(entries: &'a [SymbolEntry], query: &SymbolQuery) -> Selection<'a> {
    let wanted = name_parts(query.name.as_str());
    let named: Vec<&SymbolEntry> = entries
        .iter()
        .filter(|entry| name_parts(&entry.name).simple == wanted.simple)
        .collect();

    let required: Vec<String> = query
        .container
        .iter()
        .map(|c| container_name(c.as_str()))
        .chain(wanted.qualifier)
        .collect();
    let named_count = named.len();
    let matched: Vec<&SymbolEntry> = named
        .into_iter()
        .filter(|entry| query.kind.is_none_or(|kind| entry.kind == kind))
        .filter(|entry| {
            let containers = effective_containers(entry);
            required.iter().all(|need| containers.contains(need))
        })
        .collect();
    Selection {
        excluded_by_filters: named_count.saturating_sub(matched.len()),
        matched,
    }
}

/// Where an entry's identifier is.
enum IdentifierPosition {
    Verified(Position2D, PositionSource),
    Unverified(Position2D),
}

/// Whether the document text spells `word` at the MCP `position`.
fn spells_at(line_text: &str, character: u32, word: &str) -> bool {
    let utf16 = EncodingConverter::new(PositionEncoding::Utf16);
    let Ok(byte) = utf16.character_to_byte_offset(line_text, character.saturating_sub(1)) else {
        return false;
    };
    whole_word_occurrences(line_text, word).contains(&byte)
}

async fn locate_identifier(
    translator: &Translator,
    ctx: &EncodingCtx,
    uri: &lsp_types::Uri,
    entry: &SymbolEntry,
) -> IdentifierPosition {
    let simple = name_parts(&entry.name).simple;
    let Some(path) = uri_to_path(uri) else {
        return IdentifierPosition::Unverified(ctx.to_mcp(uri, entry.range.start).await);
    };
    let tracker = translator.document_tracker();

    if let Some(selection) = entry.selection_range {
        let position = ctx.to_mcp(uri, selection.start).await;
        let spelled = tracker
            .line_text(&path, position.line.saturating_sub(1))
            .is_some_and(|text| spells_at(&text, position.character, &simple));
        if spelled {
            return IdentifierPosition::Verified(position, PositionSource::SelectionRange);
        }
    }

    let start = ctx.to_mcp(uri, entry.range.start).await;
    let end = ctx.to_mcp(uri, entry.range.end).await;
    let last_line = end
        .line
        .min(start.line.saturating_add(MAX_WINDOW_LINES - 1));
    let utf16 = EncodingConverter::new(PositionEncoding::Utf16);
    let window = usize::try_from(last_line.saturating_sub(start.line).saturating_add(1))
        .unwrap_or(usize::MAX);
    let text = tracker
        .line_window(&path, start.line.saturating_sub(1), window)
        .unwrap_or_default();
    let mut found = Vec::new();
    for (line, text) in (start.line..).zip(text.split('\n')) {
        for byte in whole_word_occurrences(text, &simple) {
            let Ok(column) = utf16.byte_offset_to_character(text, byte) else {
                continue;
            };
            let character = column.saturating_add(1);
            let after_start = (line, character) >= (start.line, start.character);
            let before_end = (line, character) < (end.line, end.character);
            if after_start && before_end {
                found.push(Position2D { line, character });
            }
        }
        if found.len() > 1 {
            break;
        }
    }
    match <[Position2D; 1]>::try_from(found) {
        Ok([position]) => IdentifierPosition::Verified(position, PositionSource::Inferred),
        Err(_) => IdentifierPosition::Unverified(start),
    }
}

fn candidate(entry: &SymbolEntry, position: &Position2D) -> SymbolCandidate {
    SymbolCandidate {
        name: entry.name.clone(),
        kind: u32::from(entry.kind),
        kind_name: format!("{:?}", entry.kind),
        container: entry.container.clone(),
        line: position.line,
        character: position.character,
    }
}

const fn identifier_position(located: &IdentifierPosition) -> &Position2D {
    match located {
        IdentifierPosition::Verified(position, _) | IdentifierPosition::Unverified(position) => {
            position
        }
    }
}

fn resolution_error(data: SymbolResolutionData) -> Error {
    Error::SymbolResolution(Box::new(data))
}

impl Translator {
    /// Resolve how a tool is to be aimed: a position passes through
    /// untouched, a name is resolved against the file's document symbols.
    ///
    /// The routed server's capability for `tool` is checked before anything
    /// else, so a name-addressed call fails like the position form does, and
    /// no document-symbol request is made for a tool that could not run.
    ///
    /// # Errors
    ///
    /// The routing, capability and document errors of the position form, the
    /// document-symbol request's own errors, and [`Error::SymbolResolution`]
    /// when the name does not resolve to exactly one verifiable position.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::PathBuf;
    ///
    /// use mcpls_core::bridge::{
    ///     AddressableTool, ClientPath, Position, SymbolTarget, Translator,
    /// };
    ///
    /// # async fn run(translator: &Translator) -> Result<(), Box<dyn std::error::Error>> {
    /// let file = ClientPath::try_from(PathBuf::from("/ws/src/lib.rs"))?;
    /// let target = SymbolTarget::Position(Position { line: 3, character: 5 });
    /// let resolved = translator
    ///     .resolve_symbol_target(&file, target, AddressableTool::Hover)
    ///     .await?;
    /// assert!(resolved.resolved.is_none());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn resolve_symbol_target(
        &self,
        file_path: &ClientPath,
        target: SymbolTarget,
        tool: AddressableTool,
    ) -> Result<ResolvedTarget> {
        let query = match target {
            SymbolTarget::Position(position) => {
                return Ok(ResolvedTarget {
                    position,
                    resolved: None,
                });
            }
            SymbolTarget::Name(query) => query,
        };

        drop(
            self.prepare_gated_document(file_path, tool.capability(), IndexingGate::NotRequired)
                .await?,
        );
        let fetched = self.request_document_symbols(file_path).await?;
        let uri = fetched.doc.uri().clone();
        let entries = fetched.response.map(flatten_symbols).unwrap_or_default();
        let selection = select_candidates(&entries, &query);

        match selection.matched.as_slice() {
            [] => Err(self.missing_symbol_error(&query, &selection, &uri)),
            [entry] => {
                let located = locate_identifier(self, &fetched.ctx, &uri, entry).await;
                match located {
                    IdentifierPosition::Verified(position, source) => Ok(ResolvedTarget {
                        position: Position {
                            line: position.line,
                            character: position.character,
                        },
                        resolved: Some(ResolvedSymbol {
                            name: entry.name.clone(),
                            kind: u32::from(entry.kind),
                            container: entry.container.clone(),
                            position,
                            position_source: source,
                        }),
                    }),
                    IdentifierPosition::Unverified(position) => {
                        Err(resolution_error(SymbolResolutionData::PositionUnverified {
                            name: query.name.as_str().to_string(),
                            candidate: candidate(entry, &position),
                        }))
                    }
                }
            }
            many => {
                let mut candidates = Vec::with_capacity(many.len().min(MAX_CANDIDATES));
                for entry in many.iter().take(MAX_CANDIDATES) {
                    let located = locate_identifier(self, &fetched.ctx, &uri, entry).await;
                    candidates.push(candidate(entry, identifier_position(&located)));
                }
                Err(resolution_error(SymbolResolutionData::Ambiguous {
                    name: query.name.as_str().to_string(),
                    truncated: many.len() > MAX_CANDIDATES,
                    candidates,
                }))
            }
        }
    }

    /// The error for a name with no matching symbol: filtered out, merely
    /// referenced in the file, or absent.
    fn missing_symbol_error(
        &self,
        query: &SymbolQuery,
        selection: &Selection<'_>,
        uri: &lsp_types::Uri,
    ) -> Error {
        let name = query.name.as_str().to_string();
        if selection.excluded_by_filters == 0
            && self.occurs_in_document(uri, &name_parts(&name).simple)
        {
            return resolution_error(SymbolResolutionData::NotDefinedInFile { name });
        }
        resolution_error(SymbolResolutionData::NotFound {
            name,
            excluded_by_filters: selection.excluded_by_filters,
        })
    }

    /// Whether `word` occurs as a whole word in the tracked document text.
    fn occurs_in_document(&self, uri: &lsp_types::Uri, word: &str) -> bool {
        let Some(path) = uri_to_path(uri) else {
            return false;
        };
        self.document_tracker()
            .line_window(&path, 0, MAX_SCAN_LINES)
            .is_some_and(|text| {
                text.split('\n')
                    .any(|line| !whole_word_occurrences(line, word).is_empty())
            })
    }

    /// Run `call` at the position `target` resolves to for `tool`, attaching
    /// the resolved symbol to the result when the call was name-addressed.
    ///
    /// # Errors
    ///
    /// As [`Self::resolve_symbol_target`], then whatever `call` returns.
    pub async fn with_resolved_target<T, F, Fut>(
        &self,
        file_path: ClientPath,
        target: SymbolTarget,
        tool: AddressableTool,
        call: F,
    ) -> Result<Addressed<T>>
    where
        F: FnOnce(ClientPath, Position) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let resolved = self.resolve_symbol_target(&file_path, target, tool).await?;
        let result = call(file_path, resolved.position).await?;
        Ok(Addressed {
            result,
            resolved_symbol: resolved.resolved,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, deprecated)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: SymbolKind, container: Option<&str>) -> SymbolEntry {
        let range = lsp_types::Range::default();
        SymbolEntry {
            name: name.to_string(),
            kind,
            container: container.map(str::to_string),
            range,
            selection_range: Some(range),
        }
    }

    fn query(name: &str, kind: Option<SymbolKind>, container: Option<&str>) -> SymbolQuery {
        SymbolQuery {
            name: SymbolName::try_new(name).unwrap(),
            kind,
            container: container.map(|c| SymbolName::try_new(c).unwrap()),
        }
    }

    #[test]
    fn symbol_names_reject_blank_and_oversized_text() {
        assert_eq!(SymbolName::try_new("  parse ").unwrap().as_str(), "parse");
        assert_eq!(SymbolName::try_new(""), Err(SymbolNameError::Blank));
        assert_eq!(SymbolName::try_new(" \t"), Err(SymbolNameError::Blank));
        assert_eq!(
            SymbolName::try_new("x".repeat(MAX_SYMBOL_NAME_BYTES + 1)),
            Err(SymbolNameError::TooLong)
        );
        assert!(SymbolName::try_new("x".repeat(MAX_SYMBOL_NAME_BYTES)).is_ok());
    }

    #[test]
    fn name_parts_strip_generics_receivers_and_qualifiers() {
        let parts = |raw| name_parts(raw);
        assert_eq!(parts("bar").simple, "bar");
        assert_eq!(
            parts("Foo::bar"),
            NameParts {
                simple: "bar".to_string(),
                qualifier: Some("Foo".to_string())
            }
        );
        assert_eq!(parts("Vec<T>::push").qualifier.as_deref(), Some("Vec"));
        assert_eq!(parts("Foo.method").simple, "method");
        assert_eq!(
            parts("(*Server).Start"),
            NameParts {
                simple: "Start".to_string(),
                qualifier: Some("Server".to_string())
            }
        );
        assert_eq!(parts("map<K, V>").simple, "map");
    }

    #[test]
    fn generic_stripping_tracks_depth_and_leaves_operators_intact() {
        assert_eq!(strip_generics("Foo<T: Into<Vec<u8>>>"), "Foo");
        assert_eq!(strip_generics("operator<"), "operator<");
        assert_eq!(strip_generics("operator<<"), "operator<<");
        assert_eq!(strip_generics("operator<=>"), "operator<=>");
        assert_eq!(strip_generics("Foo::operator<<"), "Foo::operator<<");
        assert_eq!(strip_generics("operatorCount<T>"), "operatorCount");
        assert_eq!(strip_generics("get_operator<T>"), "get_operator");
        assert_eq!(strip_generics("Foo<T"), "Foo<T");
        assert_eq!(strip_generics("a>b"), "a>b");
    }

    #[test]
    fn container_names_unwrap_impl_blocks() {
        assert_eq!(container_name("impl Foo"), "Foo");
        assert_eq!(container_name("impl<T> Foo<T>"), "Foo");
        assert_eq!(container_name("impl<T: Into<Vec<u8>>> Foo<T>"), "Foo");
        assert_eq!(container_name("impl Display for Foo"), "Foo");
        assert_eq!(container_name("impl<T> Tr<T> for Foo<T>"), "Foo");
        assert_eq!(container_name("Config"), "Config");
        assert_eq!(container_name("implementation"), "implementation");
    }

    #[test]
    fn whole_word_search_is_unicode_aware() {
        assert_eq!(
            whole_word_occurrences("foo foo_bar xfoo foo", "foo"),
            [0, 17]
        );
        assert!(whole_word_occurrences("éfoo", "foo").is_empty());
        assert!(whole_word_occurrences("foo日本", "foo").is_empty());
        assert_eq!(whole_word_occurrences("(foo)", "foo"), [1]);
    }

    #[test]
    fn an_unqualified_name_matches_every_symbol_with_that_simple_name() {
        let entries = [
            entry("Handle", SymbolKind::Function, None),
            entry("(*Server).Handle", SymbolKind::Method, None),
            entry("Foo::new", SymbolKind::Method, None),
        ];
        let selection = select_candidates(&entries, &query("Handle", None, None));
        assert_eq!(
            selection.matched.len(),
            2,
            "a free function must not shadow a method"
        );

        let narrowed = select_candidates(&entries, &query("Handle", None, Some("Server")));
        assert_eq!(narrowed.matched.len(), 1);
        assert_eq!(narrowed.matched[0].name, "(*Server).Handle");
    }

    #[test]
    fn parameter_lists_are_stripped_so_overloads_match_by_simple_name() {
        assert_eq!(name_parts("bar(int)").simple, "bar");
        assert_eq!(name_parts("foo(java.util.List)").simple, "foo");
        assert_eq!(
            name_parts("Foo.bar(int, String)").qualifier.as_deref(),
            Some("Foo")
        );
        assert_eq!(name_parts("operator()").simple, "operator()");
        assert_eq!(name_parts("Foo::operator()").simple, "operator()");
        assert_eq!(name_parts("operatorCount(int)").simple, "operatorCount");
        assert_eq!(name_parts("get_operator(x)").simple, "get_operator");
        assert_eq!(name_parts("(*T).M(x)").simple, "M");
        let entries = [
            entry("bar(int)", SymbolKind::Method, Some("Foo")),
            entry("bar(String)", SymbolKind::Method, Some("Foo")),
            entry("Foo(int)", SymbolKind::Constructor, Some("Foo")),
        ];
        assert_eq!(
            select_candidates(&entries, &query("bar", None, None))
                .matched
                .len(),
            2
        );
        assert_eq!(
            select_candidates(&entries, &query("Foo.bar", None, None))
                .matched
                .len(),
            2
        );
        assert_eq!(
            select_candidates(&entries, &query("Foo", None, None))
                .matched
                .len(),
            1
        );
    }

    #[test]
    fn simple_name_matches_when_no_exact_name_exists() {
        let entries = [
            entry("Foo::new", SymbolKind::Method, None),
            entry("Bar::new", SymbolKind::Method, None),
        ];
        let selection = select_candidates(&entries, &query("new", None, None));
        assert_eq!(selection.matched.len(), 2);
    }

    #[test]
    fn container_and_kind_narrow_before_ambiguity() {
        let entries = [
            entry("new", SymbolKind::Method, Some("impl Foo<T>")),
            entry("new", SymbolKind::Method, Some("impl Bar")),
            entry("new", SymbolKind::Function, None),
        ];
        let by_container = select_candidates(&entries, &query("new", None, Some("Foo")));
        assert_eq!(by_container.matched.len(), 1);
        assert_eq!(by_container.excluded_by_filters, 2);

        let by_kind = select_candidates(&entries, &query("new", Some(SymbolKind::Function), None));
        assert_eq!(by_kind.matched.len(), 1);

        let by_qualifier = select_candidates(&entries, &query("Bar::new", None, None));
        assert_eq!(by_qualifier.matched.len(), 1);
        assert_eq!(
            by_qualifier.matched[0].container.as_deref(),
            Some("impl Bar")
        );
    }

    #[test]
    fn flattening_keeps_document_order_and_parent_names() {
        let range = lsp_types::Range::default();
        let child = lsp_types::DocumentSymbol {
            name: "method".to_string(),
            detail: None,
            kind: SymbolKind::Method,
            tags: None,
            deprecated: None,
            range,
            selection_range: range,
            children: None,
        };
        let parent = lsp_types::DocumentSymbol {
            name: "Type".to_string(),
            kind: SymbolKind::Struct,
            children: Some(vec![child]),
            ..lsp_types::DocumentSymbol {
                name: String::new(),
                detail: None,
                kind: SymbolKind::Struct,
                tags: None,
                deprecated: None,
                range,
                selection_range: range,
                children: None,
            }
        };
        let entries = flatten_symbols(DocumentSymbolResponse::DocumentSymbolList(vec![parent]));
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.name.as_str(), e.container.as_deref()))
                .collect::<Vec<_>>(),
            [("Type", None), ("method", Some("Type"))]
        );
    }

    #[test]
    fn spells_at_checks_the_column_in_utf16_units() {
        let line = "let 日本 = foo;";
        let col = u32::try_from(
            line.encode_utf16()
                .position(|u| u == u16::from(b'f'))
                .unwrap(),
        )
        .unwrap();
        assert!(spells_at(line, col + 1, "foo"));
        assert!(!spells_at(line, col, "foo"));
        assert!(!spells_at(line, 999, "foo"));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod resolver_tests {
    use std::sync::Arc;
    use std::{assert_matches, fs};

    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::io::BufReader;
    use url::Url;

    use super::super::testing::{
        pos, read_framed_message, translator_with_capabilities, write_response,
    };
    use super::*;
    use crate::config::ServerId;
    use crate::test_lsp::client_path;

    fn caps(hover: bool) -> lsp_types::ServerCapabilities {
        lsp_types::ServerCapabilities {
            hover_provider: hover.then_some(lsp_types::HoverProvider::Bool(true)),
            document_symbol_provider: Some(lsp_types::DocumentSymbolProvider::Bool(true)),
            ..Default::default()
        }
    }

    fn range(l1: u32, c1: u32, l2: u32, c2: u32) -> Value {
        json!({"start": {"line": l1, "character": c1}, "end": {"line": l2, "character": c2}})
    }

    fn hierarchical(
        name: &str,
        kind: u32,
        whole: Value,
        selection: Value,
        children: Value,
    ) -> Value {
        let mut symbol = json!({"name": name, "kind": kind});
        symbol["range"] = whole;
        symbol["selectionRange"] = selection;
        symbol["children"] = children;
        symbol
    }

    fn query_target(name: &str, kind: Option<SymbolKind>, container: Option<&str>) -> SymbolTarget {
        SymbolTarget::Name(SymbolQuery {
            name: SymbolName::try_new(name).unwrap(),
            kind,
            container: container.map(|c| SymbolName::try_new(c).unwrap()),
        })
    }

    /// Runs a name resolution against a fake server answering
    /// `documentSymbol` with `symbols` for a file holding `source`.
    async fn resolve(
        source: &str,
        symbols: impl FnOnce(&Url) -> Value,
        target: SymbolTarget,
    ) -> Result<ResolvedTarget> {
        let dir = TempDir::new().unwrap();
        let id = ServerId::from("rust");
        let (translator, mut server) = translator_with_capabilities(&dir, &id, caps(true));
        let path = dir.path().join("main.rs");
        fs::write(&path, source).unwrap();
        let uri = Url::from_file_path(dunce::canonicalize(&path).unwrap()).unwrap();
        let answer = symbols(&uri);

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = client_path(&path);
            tokio::spawn(async move {
                translator
                    .resolve_symbol_target(&path, target, AddressableTool::Hover)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/documentSymbol");
        write_response(&mut server.read_half_stdin, &request["id"], answer).await;
        handle.await.unwrap()
    }

    fn resolution(result: Result<ResolvedTarget>) -> SymbolResolutionData {
        match result.unwrap_err() {
            Error::SymbolResolution(data) => *data,
            other => panic!("expected a symbol resolution error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_unique_hierarchical_symbol_resolves_to_its_verified_selection_range() {
        let resolved = resolve(
            "pub fn parse_config() {}\n",
            |_| {
                json!([hierarchical(
                    "parse_config",
                    12,
                    range(0, 0, 0, 24),
                    range(0, 7, 0, 19),
                    json!([])
                )])
            },
            query_target("parse_config", None, None),
        )
        .await
        .unwrap();

        assert_eq!(resolved.position, pos(1, 8));
        let symbol = resolved.resolved.unwrap();
        assert_eq!(symbol.position_source, PositionSource::SelectionRange);
        assert_eq!(symbol.kind, 12);
    }

    #[tokio::test]
    async fn test_flat_symbol_is_located_by_its_only_whole_word_occurrence() {
        let resolved = resolve(
            "pub fn foo() {}\n",
            |uri| {
                json!([{
                    "name": "foo", "kind": 12,
                    "location": {"uri": uri.as_str(), "range": range(0, 0, 0, 15)}
                }])
            },
            query_target("foo", None, None),
        )
        .await
        .unwrap();

        assert_eq!(resolved.position, pos(1, 8));
        assert_eq!(
            resolved.resolved.unwrap().position_source,
            PositionSource::Inferred
        );
    }

    #[tokio::test]
    async fn test_a_string_literal_before_the_identifier_never_becomes_the_position() {
        let result = resolve(
            "#[cfg(feature = \"foo\")]\npub fn foo() {}\n",
            |uri| {
                json!([{
                    "name": "foo", "kind": 12,
                    "location": {"uri": uri.as_str(), "range": range(0, 0, 1, 15)}
                }])
            },
            query_target("foo", None, None),
        )
        .await;

        assert_matches!(
            resolution(result),
            SymbolResolutionData::PositionUnverified { .. }
        );
    }

    #[tokio::test]
    async fn test_a_wrong_selection_range_falls_back_to_the_unique_occurrence() {
        let resolved = resolve(
            "pub fn foo() {}\n",
            |_| {
                json!([hierarchical(
                    "foo",
                    12,
                    range(0, 0, 0, 15),
                    range(0, 0, 0, 3),
                    json!([])
                )])
            },
            query_target("foo", None, None),
        )
        .await
        .unwrap();

        assert_eq!(resolved.position, pos(1, 8));
        assert_eq!(
            resolved.resolved.unwrap().position_source,
            PositionSource::Inferred
        );
    }

    #[tokio::test]
    async fn test_same_named_methods_are_listed_with_their_positions_not_picked() {
        let result = resolve(
            "impl Foo {\n    fn new() {}\n}\nimpl Bar {\n    fn new() {}\n}\n",
            |_| {
                json!([
                    hierarchical(
                        "impl Foo",
                        5,
                        range(0, 0, 2, 1),
                        range(0, 5, 0, 8),
                        json!([hierarchical(
                            "new",
                            6,
                            range(1, 4, 1, 15),
                            range(1, 7, 1, 10),
                            json!([])
                        )])
                    ),
                    hierarchical(
                        "impl Bar",
                        5,
                        range(3, 0, 5, 1),
                        range(3, 5, 3, 8),
                        json!([hierarchical(
                            "new",
                            6,
                            range(4, 4, 4, 15),
                            range(4, 7, 4, 10),
                            json!([])
                        )])
                    ),
                ])
            },
            query_target("new", None, None),
        )
        .await;

        let SymbolResolutionData::Ambiguous {
            candidates,
            truncated,
            ..
        } = resolution(result)
        else {
            panic!("expected an ambiguity");
        };
        assert!(!truncated);
        assert_eq!(
            candidates
                .iter()
                .map(|c| (c.container.as_deref(), c.line, c.character))
                .collect::<Vec<_>>(),
            [(Some("impl Foo"), 2, 8), (Some("impl Bar"), 5, 8)]
        );
    }

    #[tokio::test]
    async fn test_container_qualifier_narrows_to_one_symbol() {
        let resolved = resolve(
            "impl<T> Foo<T> {\n    fn new() {}\n}\nimpl Bar {\n    fn new() {}\n}\n",
            |_| {
                json!([
                    hierarchical(
                        "impl<T> Foo<T>",
                        5,
                        range(0, 0, 2, 1),
                        range(0, 8, 0, 14),
                        json!([hierarchical(
                            "new",
                            6,
                            range(1, 4, 1, 15),
                            range(1, 7, 1, 10),
                            json!([])
                        )])
                    ),
                    hierarchical(
                        "impl Bar",
                        5,
                        range(3, 0, 5, 1),
                        range(3, 5, 3, 8),
                        json!([hierarchical(
                            "new",
                            6,
                            range(4, 4, 4, 15),
                            range(4, 7, 4, 10),
                            json!([])
                        )])
                    ),
                ])
            },
            query_target("new", None, Some("Foo")),
        )
        .await
        .unwrap();

        assert_eq!(resolved.position, pos(2, 8));
    }

    #[tokio::test]
    async fn test_a_method_named_with_its_parameter_list_resolves_and_overloads_are_ambiguous() {
        let source =
            "class Foo {\n  void bar(int a) {}\n  void bar(String s) {}\n  void baz(int a) {}\n}\n";
        let symbols = |_: &Url| {
            json!([hierarchical(
                "Foo",
                5,
                range(0, 0, 4, 1),
                range(0, 6, 0, 9),
                json!([
                    hierarchical(
                        "bar(int)",
                        6,
                        range(1, 2, 1, 19),
                        range(1, 7, 1, 10),
                        json!([])
                    ),
                    hierarchical(
                        "bar(String)",
                        6,
                        range(2, 2, 2, 22),
                        range(2, 7, 2, 10),
                        json!([])
                    ),
                    hierarchical(
                        "baz(int)",
                        6,
                        range(3, 2, 3, 19),
                        range(3, 7, 3, 10),
                        json!([])
                    ),
                ])
            )])
        };

        let single = resolve(source, symbols, query_target("baz", None, None))
            .await
            .unwrap();
        assert_eq!(single.position, pos(4, 8));

        let overloads = resolve(source, symbols, query_target("Foo.bar", None, None)).await;
        let SymbolResolutionData::Ambiguous { candidates, .. } = resolution(overloads) else {
            panic!("overloads must be ambiguous, never not_defined_in_file");
        };
        assert_eq!(candidates.len(), 2);
    }

    #[tokio::test]
    async fn test_a_free_function_does_not_shadow_a_same_named_method() {
        let source = "func Handle() {}\nfunc (s *Server) Handle() {}\n";
        let symbols = |_: &Url| {
            json!([
                hierarchical(
                    "Handle",
                    12,
                    range(0, 0, 0, 16),
                    range(0, 5, 0, 11),
                    json!([])
                ),
                hierarchical(
                    "(*Server).Handle",
                    6,
                    range(1, 0, 1, 30),
                    range(1, 17, 1, 23),
                    json!([])
                ),
            ])
        };

        let unqualified = resolve(source, symbols, query_target("Handle", None, None)).await;
        assert_matches!(
            resolution(unqualified),
            SymbolResolutionData::Ambiguous { .. }
        );

        let narrowed = resolve(
            source,
            symbols,
            query_target("Handle", None, Some("Server")),
        )
        .await
        .unwrap();
        assert_eq!(narrowed.position, pos(2, 18));
    }

    #[tokio::test]
    async fn test_a_name_only_referenced_in_the_file_is_not_defined_there() {
        let result = resolve(
            "use config::Config;\nfn main() {}\n",
            |_| {
                json!([hierarchical(
                    "main",
                    12,
                    range(1, 0, 1, 12),
                    range(1, 3, 1, 7),
                    json!([])
                )])
            },
            query_target("Config", None, None),
        )
        .await;

        assert_eq!(
            resolution(result),
            SymbolResolutionData::NotDefinedInFile {
                name: "Config".to_string()
            }
        );
    }

    #[tokio::test]
    async fn test_an_absent_name_is_not_found_and_a_filtered_out_one_says_so() {
        let absent = resolve(
            "fn main() {}\n",
            |_| json!([]),
            query_target("nope", None, None),
        )
        .await;
        assert_eq!(
            resolution(absent),
            SymbolResolutionData::NotFound {
                name: "nope".to_string(),
                excluded_by_filters: 0
            }
        );

        let filtered = resolve(
            "fn main() {}\n",
            |_| {
                json!([hierarchical(
                    "main",
                    12,
                    range(0, 0, 0, 12),
                    range(0, 3, 0, 7),
                    json!([])
                )])
            },
            query_target("main", Some(SymbolKind::Class), None),
        )
        .await;
        assert_eq!(
            resolution(filtered),
            SymbolResolutionData::NotFound {
                name: "main".to_string(),
                excluded_by_filters: 1
            }
        );
    }

    #[tokio::test]
    async fn test_a_position_target_passes_through_without_any_request() {
        let dir = TempDir::new().unwrap();
        let (translator, _server) =
            translator_with_capabilities(&dir, &ServerId::from("rust"), caps(false));

        let resolved = translator
            .resolve_symbol_target(
                &client_path("/never/opened.rs"),
                SymbolTarget::Position(pos(3, 4)),
                AddressableTool::Hover,
            )
            .await
            .unwrap();

        assert_eq!(resolved.position, pos(3, 4));
        assert!(resolved.resolved.is_none());
    }

    #[tokio::test]
    async fn test_a_missing_tool_capability_fails_before_any_symbol_request() {
        let dir = TempDir::new().unwrap();
        let (translator, _server) =
            translator_with_capabilities(&dir, &ServerId::from("rust"), caps(false));
        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();

        let err = translator
            .resolve_symbol_target(
                &client_path(&path),
                query_target("main", None, None),
                AddressableTool::Hover,
            )
            .await
            .unwrap_err();

        assert_matches!(
            err,
            Error::CapabilityNotSupported {
                capability: "hoverProvider",
                ..
            },
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn test_a_name_in_a_file_outside_the_workspace_fails_containment_not_resolution() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let (translator, _server) =
            translator_with_capabilities(&dir, &ServerId::from("rust"), caps(true));
        let path = outside.path().join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();

        let err = translator
            .resolve_symbol_target(
                &client_path(&path),
                query_target("main", None, None),
                AddressableTool::Hover,
            )
            .await
            .unwrap_err();

        assert_matches!(err, Error::PathOutsideWorkspace(_), "{err:?}");
    }
}
