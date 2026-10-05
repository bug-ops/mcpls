//! Redaction of secrets from server output that reaches logs and MCP clients.
//!
//! A language server's stderr is shown to the MCP client when startup fails.
//! A server that dumps its environment or echoes a rejected argument could
//! otherwise hand over credentials, so values that look secret are replaced
//! with `[redacted:NAME]` first. Matching is by exact value plus its JSON and
//! `Debug` escaped spellings, so a secret is also found inside a logged wire
//! frame or a `{:?}` rendering. Other encodings (URL, base64) are not found,
//! and neither is a JSON spelling written by a foreign encoder that escapes
//! non-ASCII as `\uXXXX` or `/` as `\/`: such a secret can slip past trace
//! redaction.

use std::borrow::Cow;

use crate::config::LspServerConfig;
use crate::error::Error;

/// Shortest value redacted; shorter ones would match unrelated text.
const MIN_SECRET_BYTES: usize = 8;

/// Shortest fragment of a secret hidden when an elision cut splits it.
const MIN_FRAGMENT_BYTES: usize = 4;

/// Longest label kept in a replacement marker.
const MAX_LABEL_CHARS: usize = 64;

/// Upper-case substrings that make an environment variable, flag or JSON key
/// name denote a secret.
const SECRET_NAME_PATTERNS: [&str; 6] = ["TOKEN", "KEY", "SECRET", "PASSW", "CRED", "AUTH"];

/// Whole name segments (upper-case) that contain a secret pattern but name
/// something benign, such as the `AUTH` of `GIT_AUTHOR_NAME`.
const BENIGN_SEGMENTS: [&str; 7] = [
    "AUTHOR",
    "AUTHORS",
    "AUTHORITY",
    "XAUTHORITY",
    "KEYBOARD",
    "KEYMAP",
    "TOKENIZERS",
];

/// Whole names (upper-case) that are benign although their segments joined
/// would match: `SSH_AUTH_SOCK` holds a socket path.
const BENIGN_NAMES: [&str; 1] = ["SSH_AUTH_SOCK"];

/// Splits `name` into segments at `_`, `-` and `.`, and at camel-case
/// boundaries (`proxyAuth`, and the end of an acronym in `XMLHttp`).
fn name_segments(name: &str) -> impl Iterator<Item = &str> {
    name.split(['_', '-', '.']).flat_map(|piece| {
        let mut starts = vec![0];
        let chars: Vec<(usize, char)> = piece.char_indices().collect();
        for (index, window) in chars.windows(2).enumerate() {
            let [(_, before), (start, after)] = window else {
                continue;
            };
            let next_is_lower = chars
                .get(index.saturating_add(2))
                .is_some_and(|(_, next)| next.is_lowercase());
            let lower_to_upper =
                (before.is_lowercase() || before.is_ascii_digit()) && after.is_uppercase();
            let acronym_end = before.is_uppercase() && after.is_uppercase() && next_is_lower;
            if lower_to_upper || acronym_end {
                starts.push(*start);
            }
        }
        starts.push(piece.len());
        starts
            .windows(2)
            .filter_map(|bounds| match bounds {
                [from, to] => piece.get(*from..*to),
                _ => None,
            })
            .collect::<Vec<_>>()
    })
}

/// Whether `name` (an environment variable, a flag without its dashes, or a
/// JSON key) denotes a secret, ignoring case.
///
/// Fails closed: the segments that are exactly a benign word are dropped, the
/// rest are joined and matched by substring, so `passWord` and `AUTHORIZATION`
/// stay secret while `GIT_AUTHOR_NAME` and `XAUTHORITY` do not.
pub fn is_secret_name(name: &str) -> bool {
    if BENIGN_NAMES.contains(&name.to_ascii_uppercase().as_str()) {
        return false;
    }
    let kept: String = name_segments(name)
        .map(str::to_ascii_uppercase)
        .filter(|segment| !BENIGN_SEGMENTS.contains(&segment.as_str()))
        .collect();
    SECRET_NAME_PATTERNS
        .iter()
        .any(|pattern| kept.contains(pattern))
}

/// Text known to carry no configured secret: it was redacted when built or is
/// a fixed string.
///
/// It has no public constructor, so a value of this type held by
/// [`Error::LspProtocolError`] cannot be an unredacted `String`; the crate
/// builds it only through the redaction set of the server it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedText(String);

impl RedactedText {
    /// Wraps a literal, which cannot hold a secret.
    pub(crate) fn fixed(text: &'static str) -> Self {
        Self(text.to_owned())
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RedactedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A tool result whose server-supplied display prose can be scrubbed of
/// configured secrets before it reaches the MCP client.
///
/// Implementors destructure `Self` exhaustively so a new field must be
/// classified: prose goes through [`Redactions::redact_in_place`], payload
/// the client reuses verbatim (edits, locations, identifiers, round-trip
/// items, command arguments) through [`Redactions::note_payload`], nested
/// results through their own `redact_server_text`, and non-text fields are
/// skipped. A string already redacted upstream (cached diagnostics, logs and
/// messages) is not redacted again.
pub trait ServerText {
    /// Redacts the prose fields of `self` in place.
    fn redact_server_text(&mut self, redactions: &Redactions);
}

impl<T: ServerText> ServerText for Vec<T> {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        for item in self {
            item.redact_server_text(redactions);
        }
    }
}

impl<T: ServerText> ServerText for Option<T> {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        if let Some(inner) = self {
            inner.redact_server_text(redactions);
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Secret {
    label: String,
    value: String,
    /// JSON- and `Debug`-escaped spellings of `value` that differ from it.
    escaped: Vec<String>,
}

/// Prints the label only: a derived `Debug` would print the secret itself
/// wherever a struct holding a [`Redactions`] is formatted.
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

impl Secret {
    fn new(label: String, value: String) -> Self {
        let json = serde_json::Value::String(value.clone()).to_string();
        let debug = format!("{value:?}");
        let mut escaped: Vec<String> = [json, debug]
            .into_iter()
            .filter_map(|quoted| quoted.strip_circumfix('"', '"').map(str::to_owned))
            .filter(|spelling| *spelling != value)
            .collect();
        escaped.sort();
        escaped.dedup();
        Self {
            label,
            value,
            escaped,
        }
    }

    fn marker(&self) -> String {
        format!("[redacted:{}]", self.label)
    }

    fn spellings(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.value.as_str()).chain(self.escaped.iter().map(String::as_str))
    }
}

/// The secret values to hide from a server's output, longest first so a value
/// that contains another is replaced whole.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Redactions(Vec<Secret>);

/// Prints the number of secrets only, never a value.
impl std::fmt::Debug for Redactions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactions")
            .field("secrets", &self.0.len())
            .finish()
    }
}

impl Redactions {
    /// Builds the set from `(label, value)` candidates, dropping values under
    /// [`MIN_SECRET_BYTES`] and duplicates. Labels are reduced to
    /// `[A-Za-z0-9_.-]`.
    pub(crate) fn new(candidates: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut secrets: Vec<Secret> = candidates
            .into_iter()
            .filter(|(_, value)| value.len() >= MIN_SECRET_BYTES)
            .map(|(label, value)| Secret::new(sanitize_label(&label), value))
            .collect();
        secrets.sort_by(|a, b| {
            b.value
                .len()
                .cmp(&a.value.len())
                .then_with(|| a.value.cmp(&b.value))
                .then_with(|| a.label.cmp(&b.label))
        });
        secrets.dedup_by(|a, b| a.value == b.value);
        let markers: Vec<String> = secrets.iter().map(Secret::marker).collect();
        secrets.retain(|secret| {
            let inside_marker = markers.iter().any(|marker| marker.contains(&secret.value));
            if inside_marker {
                tracing::warn!(
                    "secret {} is not redacted: its value occurs inside a redaction marker",
                    secret.marker()
                );
            }
            !inside_marker
        });
        Self(secrets)
    }

    /// The set hiding every secret of `sets`, rebuilt through [`Self::new`]
    /// so its marker-substring filter runs over the merged labels.
    pub(crate) fn union<'a>(sets: impl IntoIterator<Item = &'a Self>) -> Self {
        let mut sets = sets.into_iter().filter(|set| !set.is_empty());
        let Some(first) = sets.next() else {
            return Self::default();
        };
        let Some(second) = sets.next() else {
            return first.clone();
        };
        Self::new(
            [first, second]
                .into_iter()
                .chain(sets)
                .flat_map(|set| set.0.iter())
                .map(|secret| (secret.label.clone(), secret.value.clone())),
        )
    }

    /// Records, at debug level and by label only, that `text` -- a payload the
    /// client applies verbatim and so is left unmodified -- holds a secret.
    pub(crate) fn note_payload(&self, text: &str) {
        if self.is_empty() || !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        for secret in &self.0 {
            if secret.spellings().any(|spelling| text.contains(spelling)) {
                tracing::debug!(
                    "tool result payload holds configured secret {}; left unmodified",
                    secret.marker()
                );
            }
        }
    }

    /// [`Self::note_payload`] for a JSON payload.
    pub(crate) fn note_payload_json(&self, value: &serde_json::Value) {
        if self.is_empty() || !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        self.note_payload(&value.to_string());
    }

    /// The secrets a launched server can leak: values of `config.env` and of
    /// the `inherited` environment whose names look secret, the value of a
    /// secret-named `--flag=value` or `--flag value` argument, and string
    /// leaves of `initialization_options` and `settings` under a secret-named
    /// key.
    pub(crate) fn for_server(
        config: &LspServerConfig,
        inherited: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self::for_servers(std::iter::once(config), inherited)
    }

    /// The secrets any of `configs` can leak, over one `inherited`
    /// environment: [`Self::for_server`] for every server, whether or not
    /// its project markers matched, so a server that echoes another's secret
    /// still has it hidden. Fails closed: a configured secret is hidden
    /// everywhere, not only in the output of the server that owns it.
    pub(crate) fn for_servers<'a>(
        configs: impl IntoIterator<Item = &'a LspServerConfig>,
        inherited: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let inherited: Vec<(String, String)> = inherited
            .into_iter()
            .filter(|(name, _)| is_secret_name(name))
            .collect();
        let mut candidates = inherited;
        for config in configs {
            candidates.extend(
                config
                    .env
                    .iter()
                    .filter(|(name, _)| is_secret_name(name))
                    .map(|(name, value)| (name.clone(), value.clone())),
            );
            collect_secret_args(&config.args, &mut candidates);
            if let Some(options) = &config.initialization_options {
                collect_secret_json(options, None, &mut candidates);
            }
            if let Some(settings) = &config.settings {
                collect_secret_json(&settings.to_value(), None, &mut candidates);
            }
        }
        Self::new(candidates)
    }

    /// Replaces every secret, in raw, JSON-escaped and `Debug`-escaped
    /// spelling, in `text` with its marker; borrows `text` when none occurs.
    pub(crate) fn apply<'a>(&self, text: &'a str) -> Cow<'a, str> {
        self.0.iter().fold(Cow::Borrowed(text), |acc, secret| {
            secret.spellings().fold(acc, |acc, spelling| {
                if acc.contains(spelling) {
                    Cow::Owned(acc.replace(spelling, &secret.marker()))
                } else {
                    acc
                }
            })
        })
    }

    /// Whether there is nothing to hide.
    pub(crate) const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// [`Self::apply`] in place; `text` is written only when a secret occurs.
    pub(crate) fn redact_in_place(&self, text: &mut String) {
        if self.is_empty() {
            return;
        }
        if let Cow::Owned(redacted) = self.apply(text) {
            *text = redacted;
        }
    }

    /// Redacts every string leaf of `value`; object keys are left alone.
    pub(crate) fn redact_json(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => self.redact_in_place(text),
            serde_json::Value::Array(items) => {
                for item in items {
                    self.redact_json(item);
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values_mut() {
                    self.redact_json(item);
                }
            }
            _ => {}
        }
    }

    /// Redacts the text a server controls in `diagnostic`: message, source,
    /// string code, related-information messages and `data`.
    ///
    /// URIs (`relatedInformation[].location.uri`, `codeDescription.href`) are
    /// kept: they are cache keys and links, and a secret-valued path would
    /// make them unusable.
    pub(crate) fn redact_diagnostic(&self, diagnostic: &mut lsp_types::Diagnostic) {
        match &mut diagnostic.message {
            lsp_types::Message::String(text) => self.redact_in_place(text),
            lsp_types::Message::MarkupContent(content) => self.redact_in_place(&mut content.value),
        }
        if let Some(source) = &mut diagnostic.source {
            self.redact_in_place(source);
        }
        if let Some(lsp_types::Code::String(code)) = &mut diagnostic.code {
            self.redact_in_place(code);
        }
        for related in diagnostic.related_information.iter_mut().flatten() {
            self.redact_in_place(&mut related.message);
        }
        if let Some(data) = &mut diagnostic.data {
            self.redact_json(data);
        }
    }

    /// Redacts the `title` and `message` of a `$/progress` payload.
    pub(crate) fn redact_progress(&self, value: &mut serde_json::Value) {
        for key in ["title", "message"] {
            if let Some(serde_json::Value::String(text)) = value.get_mut(key) {
                self.redact_in_place(text);
            }
        }
    }

    /// [`Self::apply`], wrapped as [`RedactedText`].
    fn redact(&self, text: &str) -> RedactedText {
        RedactedText(self.apply(text).into_owned())
    }

    /// An [`Error::LspProtocolError`] whose text is `message` with the secrets
    /// replaced: the only way to build one from formatted text.
    pub(crate) fn protocol_error(&self, message: std::fmt::Arguments<'_>) -> Error {
        Error::LspProtocolError(self.redact(&message.to_string()))
    }

    /// Hides a secret cut by an elision boundary at the end of `head`: the
    /// longest trailing fragment of at least [`MIN_FRAGMENT_BYTES`] bytes that
    /// is a prefix of a secret is replaced by its marker.
    pub(crate) fn mask_cut_head(&self, head: &str) -> String {
        for secret in &self.0 {
            for len in (MIN_FRAGMENT_BYTES..secret.value.len()).rev() {
                let Some(fragment) = secret.value.get(..len) else {
                    continue;
                };
                if let Some(kept) = head.strip_suffix(fragment) {
                    return format!("{kept}{}", secret.marker());
                }
            }
        }
        head.to_owned()
    }

    /// As [`Self::mask_cut_head`], for a secret cut at the start of `tail`.
    pub(crate) fn mask_cut_tail(&self, tail: &str) -> String {
        for secret in &self.0 {
            let total = secret.value.len();
            for len in (MIN_FRAGMENT_BYTES..total).rev() {
                let Some(fragment) = secret.value.get(total.saturating_sub(len)..) else {
                    continue;
                };
                if let Some(rest) = tail.strip_prefix(fragment) {
                    return format!("{}{rest}", secret.marker());
                }
            }
        }
        tail.to_owned()
    }
}

fn sanitize_label(label: &str) -> String {
    label
        .chars()
        .take(MAX_LABEL_CHARS)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn collect_secret_args(args: &[String], out: &mut Vec<(String, String)>) {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let Some(flag) = arg.strip_prefix('-') else {
            continue;
        };
        let flag = flag.trim_start_matches('-');
        if let Some((name, value)) = flag.split_once('=') {
            if is_secret_name(name) {
                out.push((name.to_owned(), value.to_owned()));
            }
        } else if is_secret_name(flag)
            && let Some(value) = iter.next_if(|next| !next.starts_with('-'))
        {
            out.push((flag.to_owned(), value.clone()));
        }
    }
}

fn collect_secret_json(
    value: &serde_json::Value,
    secret_key: Option<&str>,
    out: &mut Vec<(String, String)>,
) {
    match value {
        serde_json::Value::String(text) => {
            if let Some(key) = secret_key {
                out.push((key.to_owned(), text.clone()));
            }
        }
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let key_for_child = if is_secret_name(key) {
                    Some(key.as_str())
                } else {
                    secret_key
                };
                collect_secret_json(child, key_for_child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_secret_json(item, secret_key, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::assert_matches;
    use std::collections::HashMap;

    use super::*;

    fn redactions(pairs: &[(&str, &str)]) -> Redactions {
        Redactions::new(
            pairs
                .iter()
                .map(|(label, value)| ((*label).to_owned(), (*value).to_owned())),
        )
    }

    fn server_config() -> LspServerConfig {
        let mut config = LspServerConfig::rust_analyzer();
        config.env = HashMap::new();
        config.args = Vec::new();
        config.initialization_options = None;
        config
    }

    #[test]
    fn test_for_servers_covers_every_configured_server_over_one_environment() {
        let mut first = server_config();
        first
            .env
            .insert("FIRST_TOKEN".into(), "first-secret-value".into());
        let mut second = server_config();
        second.args = vec!["--api-key=second-secret-value".into()];

        let set = Redactions::for_servers(
            [&first, &second],
            [(
                "GITHUB_TOKEN".to_owned(),
                "inherited-secret-value".to_owned(),
            )],
        );

        for text in [
            "first-secret-value",
            "second-secret-value",
            "inherited-secret-value",
        ] {
            assert!(set.apply(text).contains("[redacted:"), "{text}");
        }
    }

    #[test]
    fn test_debug_never_prints_a_secret_value() {
        let set = redactions(&[("API_TOKEN", "ghp_abcdefgh")]);
        let other_value = redactions(&[("API_TOKEN", "zzzz_ijklmnop")]);

        let printed = format!("{set:?} {:?}", set.0[0]);
        let printed_other = format!("{other_value:?} {:?}", other_value.0[0]);

        assert!(printed.eq(&printed_other));
        assert!(printed.contains("secrets: 1"));
    }

    #[test]
    fn test_new_drops_value_contained_in_a_marker() {
        let set = redactions(&[("API_TOKEN", "redacted:API_TOKEN"), ("OTHER_KEY", SECRET)]);
        assert_eq!(set.apply("redacted:API_TOKEN"), "redacted:API_TOKEN");
        assert_eq!(set.apply(SECRET), "[redacted:OTHER_KEY]");
    }

    #[test]
    fn test_union_merges_sets_and_reruns_the_marker_filter() {
        let a = redactions(&[("A_TOKEN", "alpha-secret-111")]);
        let b = redactions(&[("B_TOKEN", "A_TOKEN]-beta-2")]);
        let merged = Redactions::union([&a, &b, &Redactions::default()]);
        assert_eq!(merged.apply("alpha-secret-111"), "[redacted:A_TOKEN]");
        assert_eq!(merged.apply("A_TOKEN]-beta-2"), "[redacted:B_TOKEN]");
        assert!(Redactions::union([&Redactions::default()]).is_empty());
        assert_eq!(Redactions::union([&a]), a);
    }

    fn captured(level: tracing_subscriber::filter::LevelFilter, run: impl FnOnce()) -> Vec<String> {
        use tracing_subscriber::prelude::*;

        let logs = crate::test_lsp::CapturedLogs::default();
        let subscriber = tracing_subscriber::registry()
            .with(level)
            .with(logs.clone());
        tracing::subscriber::with_default(subscriber, run);
        logs.messages()
    }

    #[test]
    fn test_note_payload_logs_label_only_at_debug_and_is_silent_at_info() {
        use tracing_subscriber::filter::LevelFilter;

        let set = redactions(&[("API_TOKEN", SECRET)]);

        let debug = captured(LevelFilter::DEBUG, || {
            set.note_payload(&format!("let k = {SECRET};"));
            set.note_payload("clean text");
        });
        assert_eq!(debug.len(), 1, "{debug:?}");
        assert!(debug[0].contains("[redacted:API_TOKEN]"), "{debug:?}");
        assert!(!debug[0].contains(SECRET), "{debug:?}");

        let info = captured(LevelFilter::INFO, || {
            set.note_payload(&format!("let k = {SECRET};"));
        });
        assert!(info.is_empty(), "{info:?}");
    }

    #[test]
    fn test_new_warns_with_label_only_when_a_value_is_dropped() {
        use tracing_subscriber::filter::LevelFilter;

        let warnings = captured(LevelFilter::WARN, || {
            let _ = redactions(&[("API_TOKEN", "redacted:API_TOKEN")]);
        });
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("[redacted:API_TOKEN]"), "{warnings:?}");
    }

    #[test]
    fn test_for_server_collects_secret_named_settings_leaves() {
        let mut config = server_config();
        config.settings = Some(
            serde_json::from_value(serde_json::json!({
                "tool.apiToken": "settings-secret-123",
                "tool.theme": "plain-visible-value"
            }))
            .unwrap(),
        );

        let set = Redactions::for_server(&config, []);

        assert_eq!(
            set.apply("settings-secret-123 plain-visible-value"),
            "[redacted:apiToken] plain-visible-value"
        );
    }

    #[test]
    fn test_apply_finds_json_and_debug_escaped_spellings() {
        let set = redactions(&[("API_TOKEN", "pa\"ss\\word-12345")]);
        let frame = serde_json::json!({"token": "pa\"ss\\word-12345"}).to_string();
        let debug = format!("{:?}", ["pa\"ss\\word-12345"]);

        for text in [frame, debug, "raw pa\"ss\\word-12345".to_owned()] {
            let cleaned = set.apply(&text);
            assert!(cleaned.contains("[redacted:API_TOKEN]"), "{cleaned}");
            assert!(!cleaned.contains("word-12345"), "{cleaned}");
        }
    }

    const SECRET: &str = "SuperSecretValue123";

    fn secret_diagnostic() -> lsp_types::Diagnostic {
        serde_json::from_value(serde_json::json!({
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "code": "E-SuperSecretValue123",
            "codeDescription": {"href": "https://example.com/SuperSecretValue123"},
            "source": "lint-SuperSecretValue123",
            "message": {"kind": "markdown", "value": "see `SuperSecretValue123`"},
            "relatedInformation": [{
                "location": {
                    "uri": "file:///SuperSecretValue123/a.rs",
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}
                },
                "message": "defined with SuperSecretValue123"
            }],
            "data": {"SuperSecretValue123": ["x", {"nested": "has SuperSecretValue123"}], "n": 3}
        }))
        .unwrap()
    }

    #[test]
    fn test_redact_diagnostic_covers_text_fields_and_data_values() {
        let set = redactions(&[("API_TOKEN", SECRET)]);
        let mut diagnostic = secret_diagnostic();

        set.redact_diagnostic(&mut diagnostic);

        let lsp_types::Message::MarkupContent(content) = &diagnostic.message else {
            panic!("expected markup content");
        };
        assert_eq!(content.value, "see `[redacted:API_TOKEN]`");
        assert_eq!(
            diagnostic.source.as_deref(),
            Some("lint-[redacted:API_TOKEN]")
        );
        assert_eq!(
            diagnostic.code,
            Some(lsp_types::Code::String("E-[redacted:API_TOKEN]".to_owned()))
        );
        let related = diagnostic.related_information.as_ref().unwrap();
        assert_eq!(related[0].message, "defined with [redacted:API_TOKEN]");
        let data = diagnostic.data.as_ref().unwrap();
        assert_eq!(data[SECRET][1]["nested"], "has [redacted:API_TOKEN]");
        assert_eq!(data["n"], 3);
    }

    #[test]
    fn test_redact_diagnostic_keeps_uris() {
        let set = redactions(&[("API_TOKEN", SECRET)]);
        let mut diagnostic = secret_diagnostic();

        set.redact_diagnostic(&mut diagnostic);

        let related = diagnostic.related_information.as_ref().unwrap();
        assert_eq!(
            AsRef::<str>::as_ref(&related[0].location.uri),
            "file:///SuperSecretValue123/a.rs"
        );
        let href = &diagnostic.code_description.as_ref().unwrap().href;
        assert_eq!(
            AsRef::<str>::as_ref(href),
            "https://example.com/SuperSecretValue123"
        );
    }

    #[test]
    fn test_redact_progress_covers_title_and_message() {
        let set = redactions(&[("API_TOKEN", SECRET)]);
        let mut value = serde_json::json!({
            "kind": "begin", "title": "index SuperSecretValue123", "message": "at SuperSecretValue123"
        });

        set.redact_progress(&mut value);

        assert_eq!(value["title"], "index [redacted:API_TOKEN]");
        assert_eq!(value["message"], "at [redacted:API_TOKEN]");
    }

    #[test]
    fn test_redact_in_place_leaves_clean_text_untouched() {
        let set = redactions(&[("API_TOKEN", SECRET)]);
        let mut clean = String::from("nothing here");
        let mut dirty = format!("x {SECRET}");

        set.redact_in_place(&mut clean);
        set.redact_in_place(&mut dirty);

        assert_eq!(clean, "nothing here");
        assert_eq!(dirty, "x [redacted:API_TOKEN]");
        assert!(!set.is_empty());
        assert!(Redactions::default().is_empty());
    }

    #[test]
    fn test_protocol_error_masks_secrets_and_keeps_other_text() {
        let set = redactions(&[("API_TOKEN", "ghp_abcdefgh")]);

        let error = set.protocol_error(format_args!("bad value ghp_abcdefgh in {}", "frame"));

        assert_eq!(
            error.to_string(),
            "LSP protocol error: bad value [redacted:API_TOKEN] in frame"
        );
    }

    #[test]
    fn test_apply_borrows_text_without_secrets() {
        let set = redactions(&[("API_TOKEN", "ghp_abcdefgh")]);

        assert_matches!(set.apply("nothing here"), Cow::Borrowed(_));
    }

    #[test]
    fn test_secret_name_matches_case_insensitively() {
        for name in [
            "GITHUB_TOKEN",
            "api_key",
            "MySecret",
            "DB_PASSWORD",
            "AWS_CREDENTIALS",
            "Authorization",
        ] {
            assert!(is_secret_name(name), "{name}");
        }
        for name in ["RUSTUP_TOOLCHAIN", "JAVA_HOME", "GOFLAGS", "PATH"] {
            assert!(!is_secret_name(name), "{name}");
        }
    }

    #[test]
    fn test_secret_name_stays_fail_closed_across_segment_boundaries() {
        for name in [
            "AUTHORIZATION",
            "HTTP_PROXY_AUTHORIZATION",
            "http.proxyAuthorization",
            "GIT_AUTHOR_TOKEN",
            "OPENAI_API_KEY",
            "apiKey",
            "APIKEY",
            "AUTHTOKEN",
            "db_passwd",
            "passWord",
            "passWd",
            "dbPassWord",
            "myToKen",
            "XMLHttpKey",
            "SSH_AUTH_SOCK_TOKEN",
            "AUTHORS_KEY",
            "tokenizers_apikey",
            "ключ_TOKEN",
        ] {
            assert!(is_secret_name(name), "{name}");
        }
    }

    #[test]
    fn test_secret_name_skips_well_known_benign_names() {
        for name in [
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "SSH_AUTH_SOCK",
            "ssh_auth_sock",
            "XAUTHORITY",
            "XAuthority",
            "TOKENIZERS_PARALLELISM",
            "KEYBOARD_LAYOUT",
            "keymap",
        ] {
            assert!(!is_secret_name(name), "{name}");
        }
    }

    #[test]
    fn test_name_segments_split_on_separators_and_camel_case() {
        let segments = |name| name_segments(name).collect::<Vec<_>>();
        assert_eq!(
            segments("GIT_AUTHOR-NAME.x"),
            ["GIT", "AUTHOR", "NAME", "x"]
        );
        assert_eq!(segments("proxyAuthorization"), ["proxy", "Authorization"]);
        assert_eq!(segments("XMLHttpKey"), ["XML", "Http", "Key"]);
        assert_eq!(segments("v2Key"), ["v2", "Key"]);
        assert_eq!(segments(""), [""]);
    }

    #[test]
    fn test_apply_labels_the_replacement_and_skips_short_values() {
        let set = redactions(&[("GITHUB_TOKEN", "ghp_abcdefgh"), ("SHORT_KEY", "abc")]);

        assert_eq!(
            set.apply("t=ghp_abcdefgh k=abc"),
            "t=[redacted:GITHUB_TOKEN] k=abc"
        );
    }

    #[test]
    fn test_apply_replaces_the_longer_overlapping_secret_whole() {
        let set = redactions(&[("A_KEY", "abcdefgh"), ("B_KEY", "abcdefghXYZ12345")]);

        let text = set.apply("v=abcdefghXYZ12345");

        assert_eq!(text, "v=[redacted:B_KEY]");
    }

    #[test]
    fn test_new_deduplicates_equal_values() {
        let set = redactions(&[("A_TOKEN", "samesecret"), ("B_TOKEN", "samesecret")]);

        assert_eq!(set.0.len(), 1);
    }

    #[test]
    fn test_label_is_reduced_to_safe_characters() {
        let set = redactions(&[("bad]\n[label", "secretvalue")]);

        assert_eq!(set.apply("secretvalue"), "[redacted:bad___label]");
    }

    #[test]
    fn test_for_server_takes_secret_named_env_only() {
        let mut config = server_config();
        config.env = HashMap::from([
            ("API_TOKEN".to_string(), "tok-1234567890".to_string()),
            (
                "RUSTUP_TOOLCHAIN".to_string(),
                "nightly-2024-01-01".to_string(),
            ),
        ]);

        let set = Redactions::for_server(&config, []);

        let text = set.apply("tok-1234567890 nightly-2024-01-01");
        assert_eq!(text, "[redacted:API_TOKEN] nightly-2024-01-01");
    }

    #[test]
    fn test_for_server_takes_inherited_secret_env() {
        let set = Redactions::for_server(
            &server_config(),
            [
                ("GITHUB_TOKEN".to_string(), "ghp_inherited123".to_string()),
                ("HOME".to_string(), "/home/someone-long".to_string()),
            ],
        );

        assert_eq!(
            set.apply("ghp_inherited123 /home/someone-long"),
            "[redacted:GITHUB_TOKEN] /home/someone-long"
        );
    }

    #[test]
    fn test_for_server_takes_secret_flag_values_in_both_forms() {
        let mut config = server_config();
        config.args = [
            "--stdio",
            "--api-key=key-value-1",
            "--auth-token",
            "tok-value-2",
            "--verbose",
            "--log-level=trace-level",
        ]
        .map(String::from)
        .to_vec();

        let set = Redactions::for_server(&config, []);

        assert_eq!(
            set.apply("key-value-1 tok-value-2 trace-level"),
            "[redacted:api-key] [redacted:auth-token] trace-level"
        );
    }

    #[test]
    fn test_for_server_takes_string_leaves_under_secret_json_keys() {
        let mut config = server_config();
        config.initialization_options = Some(serde_json::json!({
            "server": {"name": "long-server-name", "apiToken": "init-token-value"},
            "credentials": ["first-credential", {"inner": "second-credential"}],
            "retries": 3
        }));

        let set = Redactions::for_server(&config, []);

        let text =
            set.apply("long-server-name init-token-value first-credential second-credential");
        assert_eq!(
            text,
            "long-server-name [redacted:apiToken] [redacted:credentials] [redacted:credentials]"
        );
    }

    #[test]
    fn test_mask_cut_head_hides_a_secret_prefix_at_the_boundary() {
        let set = redactions(&[("A_TOKEN", "supersecretvalue")]);

        assert_eq!(
            set.mask_cut_head("token=supersec"),
            "token=[redacted:A_TOKEN]"
        );
        assert_eq!(set.mask_cut_head("token=sup"), "token=sup");
        assert_eq!(set.mask_cut_head("nothing here"), "nothing here");
    }

    #[test]
    fn test_mask_cut_tail_hides_a_secret_suffix_at_the_boundary() {
        let set = redactions(&[("A_TOKEN", "supersecretvalue")]);

        assert_eq!(
            set.mask_cut_tail("retvalue and more"),
            "[redacted:A_TOKEN] and more"
        );
        assert_eq!(set.mask_cut_tail("lue and more"), "lue and more");
    }
}
